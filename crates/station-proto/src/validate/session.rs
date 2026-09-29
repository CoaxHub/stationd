// SPDX-License-Identifier: MIT
//! The stateful part of §5, driven by the hub gateway for one connection.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use prost::Message as _;

use super::fields::check_message;
use super::negotiate::negotiate;
use super::rate::{Sustained, TokenBucket, media_capacity};
use super::waterfall::WaterfallDecoder;
use super::{CommandKind, DropReason, MessageKind, PayloadKind, Verdict, ViolationKind};
use crate::limits;
use crate::v1;

/// The hub sent a request it must not send (a hub bug, not a station fault).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RequestError {
    /// Requests may only be sent after the station's `Hello` was accepted.
    #[error("no request may be sent before Hello is accepted")]
    NotEstablished,
    /// `request_id` must be at least 1 (§4).
    #[error("request_id must not be 0")]
    ZeroRequestId,
    /// `request_id`s must be strictly increasing (§4).
    #[error("request_id {got} is not greater than the previous {last}")]
    NotIncreasing {
        /// The previous id.
        last: u64,
        /// The id that was offered.
        got: u64,
    },
    /// At most [`limits::MAX_OUTSTANDING_REQUESTS`] may be outstanding (§4).
    #[error("{0} requests are already outstanding")]
    TooManyOutstanding(usize),
    /// The request has no command.
    #[error("request has no command")]
    MissingCommand,
    /// `OpenChannel` for a channel that is already open.
    #[error("channel {0} is already open")]
    ChannelInUse(u32),
    /// `SubscribeWaterfall` for a subscription that is already open.
    #[error("subscription {0} is already open")]
    SubscriptionInUse(u32),
}

/// Requests whose deadline passed, from [`Session::expire_requests`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpiredRequests {
    /// The request ids that timed out, oldest first. The hub treats them as
    /// failed (§2.1).
    pub request_ids: Vec<u64>,
    /// Set when [`limits::MAX_CONSECUTIVE_RESPONSE_TIMEOUTS`] timeouts
    /// happened in a row: end the session.
    pub violation: Option<ViolationKind>,
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    GetCapabilities,
    OpenChannel {
        channel_id: u32,
    },
    UpdateChannel {
        channel_id: u32,
    },
    CloseChannel,
    SetFrontend,
    SubscribeWaterfall {
        subscription_id: u32,
        bins: u32,
        row_interval_ms: u32,
    },
    UnsubscribeWaterfall,
    Ping {
        nonce: u64,
    },
    AdminOp,
}

impl Pending {
    fn from_command(c: &v1::request::Command) -> Self {
        use v1::request::Command;
        match c {
            Command::GetCapabilities(_) => Self::GetCapabilities,
            Command::OpenChannel(o) => Self::OpenChannel {
                channel_id: o.channel_id,
            },
            Command::UpdateChannel(u) => Self::UpdateChannel {
                channel_id: u.channel_id,
            },
            Command::CloseChannel(_) => Self::CloseChannel,
            Command::SetFrontend(_) => Self::SetFrontend,
            Command::SubscribeWaterfall(s) => Self::SubscribeWaterfall {
                subscription_id: s.subscription_id,
                bins: s.bins,
                row_interval_ms: s.row_interval_ms,
            },
            Command::UnsubscribeWaterfall(_) => Self::UnsubscribeWaterfall,
            Command::Ping(p) => Self::Ping { nonce: p.nonce },
            Command::AdminOp(_) => Self::AdminOp,
        }
    }

    fn kind(self) -> CommandKind {
        match self {
            Self::GetCapabilities => CommandKind::GetCapabilities,
            Self::OpenChannel { .. } => CommandKind::OpenChannel,
            Self::UpdateChannel { .. } => CommandKind::UpdateChannel,
            Self::CloseChannel => CommandKind::CloseChannel,
            Self::SetFrontend => CommandKind::SetFrontend,
            Self::SubscribeWaterfall { .. } => CommandKind::SubscribeWaterfall,
            Self::UnsubscribeWaterfall => CommandKind::UnsubscribeWaterfall,
            Self::Ping { .. } => CommandKind::Ping,
            Self::AdminOp => CommandKind::AdminOp,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Outstanding {
    pending: Pending,
    deadline: Instant,
}

#[derive(Debug, Clone)]
struct Channel {
    closed_at: Option<Instant>,
    audio: TokenBucket,
    audio_excess: Sustained,
    signal: TokenBucket,
}

impl Channel {
    fn open(frame_duration_ms: u32, now: Instant) -> Self {
        Self {
            closed_at: None,
            audio: audio_bucket(frame_duration_ms, now),
            audio_excess: Sustained::default(),
            signal: TokenBucket::new(
                f64::from(limits::MAX_SIGNALS_PER_SECOND),
                f64::from(limits::MAX_SIGNALS_PER_SECOND),
                now,
            ),
        }
    }
}

/// §5 `AudioFrame` rate: 1.5 × (1000 / `frame_duration_ms`) per second. An
/// unknown or invalid duration uses the shortest allowed one (most lenient).
fn audio_bucket(frame_duration_ms: u32, now: Instant) -> TokenBucket {
    let fd = if limits::AUDIO_FRAME_DURATIONS_MS.contains(&frame_duration_ms) {
        frame_duration_ms
    } else {
        limits::AUDIO_FRAME_DURATIONS_MS
            .iter()
            .copied()
            .min()
            .unwrap_or(10)
    };
    let per_sec = limits::MEDIA_RATE_TOLERANCE * 1000.0 / f64::from(fd);
    TokenBucket::new(per_sec, media_capacity(per_sec), now)
}

#[derive(Debug, Clone, Copy)]
struct RowParams {
    bins: u32,
    compression: i32,
}

#[derive(Debug, Clone)]
struct Subscription {
    closed_at: Option<Instant>,
    /// From `WaterfallSubscribed`; rows before it are dropped.
    params: Option<RowParams>,
    rows: Option<TokenBucket>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdState {
    Open,
    Grace,
    Unknown,
}

fn id_state(closed_at: Option<Instant>, now: Instant) -> IdState {
    match closed_at {
        None => IdState::Open,
        Some(t) if now.saturating_duration_since(t) <= limits::ID_CLOSE_GRACE => IdState::Grace,
        Some(_) => IdState::Unknown,
    }
}

/// Hub-side validation state of one station connection (§2, §4, §5).
///
/// The gateway creates it when the stream opens and drives it with:
/// - [`check`](Self::check) for every message the station sends;
/// - [`on_welcome_sent`](Self::on_welcome_sent) after it sends `Welcome`;
/// - [`on_request_sent`](Self::on_request_sent) for every request it sends,
///   which records the request and the channel or subscription IDs it opens
///   or closes (closing starts a 5 s grace period);
/// - [`expire_requests`](Self::expire_requests),
///   [`hello_timed_out`](Self::hello_timed_out) and
///   [`idle_timed_out`](Self::idle_timed_out), woken at
///   [`next_deadline`](Self::next_deadline).
///
/// All time comes from the `now` arguments (use
/// `tokio::time::Instant::into_std` under Tokio's paused clock). No method
/// panics, whatever the station sends. IDs open as soon as the hub sends the
/// request (media may race the response); a non-`OK` response to
/// `OpenChannel`/`SubscribeWaterfall` removes them again.
#[derive(Debug)]
pub struct Session {
    started_at: Instant,
    last_message_at: Instant,
    version: Option<v1::ProtocolVersion>,
    status_interval: Duration,
    max_message_bytes: usize,
    last_request_id: u64,
    outstanding: BTreeMap<u64, Outstanding>,
    timed_out: VecDeque<u64>,
    consecutive_timeouts: u32,
    channels: HashMap<u32, Channel>,
    subscriptions: HashMap<u32, Subscription>,
    status: TokenBucket,
    events: TokenBucket,
    decoder: WaterfallDecoder,
    row: Vec<u8>,
}

impl Session {
    /// A session for a stream that opened at `now`, waiting for `Hello`.
    pub fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            last_message_at: now,
            version: None,
            status_interval: limits::DEFAULT_STATUS_INTERVAL,
            max_message_bytes: limits::MAX_MESSAGE_BYTES,
            last_request_id: 0,
            outstanding: BTreeMap::new(),
            timed_out: VecDeque::new(),
            consecutive_timeouts: 0,
            channels: HashMap::new(),
            subscriptions: HashMap::new(),
            status: status_bucket(limits::DEFAULT_STATUS_INTERVAL, now),
            events: TokenBucket::new(
                f64::from(limits::EVENT_RATE_PER_SECOND),
                f64::from(limits::EVENT_BURST),
                now,
            ),
            decoder: WaterfallDecoder::new(),
            row: Vec::new(),
        }
    }

    /// The negotiated version, once `Hello` was accepted.
    pub fn negotiated_version(&self) -> Option<v1::ProtocolVersion> {
        self.version
    }

    /// Records the `Welcome` the hub sent: `status_interval_ms` (0 = default)
    /// drives the `StationStatus` rate limit, and `max_message_bytes`
    /// (0 = default, capped at [`limits::MAX_MESSAGE_BYTES`]) the size check.
    pub fn on_welcome_sent(&mut self, welcome: &v1::Welcome, now: Instant) {
        self.status_interval = match welcome.status_interval_ms {
            0 => limits::DEFAULT_STATUS_INTERVAL,
            ms => Duration::from_millis(u64::from(ms)),
        };
        self.status = status_bucket(self.status_interval, now);
        self.max_message_bytes = match welcome.max_message_bytes {
            0 => limits::MAX_MESSAGE_BYTES,
            n => usize::try_from(n)
                .unwrap_or(usize::MAX)
                .min(limits::MAX_MESSAGE_BYTES),
        };
    }

    /// Records a request the hub is about to send. Call it before writing the
    /// request to the stream; on `Err` do not send it.
    pub fn on_request_sent(&mut self, req: &v1::Request, now: Instant) -> Result<(), RequestError> {
        use v1::request::Command;
        if self.version.is_none() {
            return Err(RequestError::NotEstablished);
        }
        if req.request_id == 0 {
            return Err(RequestError::ZeroRequestId);
        }
        if req.request_id <= self.last_request_id {
            return Err(RequestError::NotIncreasing {
                last: self.last_request_id,
                got: req.request_id,
            });
        }
        if self.outstanding.len() >= limits::MAX_OUTSTANDING_REQUESTS {
            return Err(RequestError::TooManyOutstanding(self.outstanding.len()));
        }
        let command = req.command.as_ref().ok_or(RequestError::MissingCommand)?;
        self.collect_garbage(now);
        match command {
            Command::OpenChannel(o) => {
                if self.channel_state(o.channel_id, now) == IdState::Open {
                    return Err(RequestError::ChannelInUse(o.channel_id));
                }
                let fd = o.audio.as_ref().map_or(0, |a| a.frame_duration_ms);
                self.channels.insert(o.channel_id, Channel::open(fd, now));
            }
            Command::CloseChannel(c) => {
                if let Some(ch) = self.channels.get_mut(&c.channel_id) {
                    ch.closed_at.get_or_insert(now);
                }
            }
            Command::SubscribeWaterfall(s) => {
                if self.subscription_state(s.subscription_id, now) == IdState::Open {
                    return Err(RequestError::SubscriptionInUse(s.subscription_id));
                }
                self.subscriptions.insert(
                    s.subscription_id,
                    Subscription {
                        closed_at: None,
                        params: None,
                        rows: None,
                    },
                );
            }
            Command::UnsubscribeWaterfall(u) => {
                if let Some(sub) = self.subscriptions.get_mut(&u.subscription_id) {
                    sub.closed_at.get_or_insert(now);
                }
            }
            Command::GetCapabilities(_)
            | Command::UpdateChannel(_)
            | Command::SetFrontend(_)
            | Command::Ping(_)
            | Command::AdminOp(_) => {}
        }
        let pending = Pending::from_command(command);
        let deadline = now.checked_add(pending.kind().timeout()).unwrap_or(now);
        self.outstanding
            .insert(req.request_id, Outstanding { pending, deadline });
        self.last_request_id = req.request_id;
        Ok(())
    }

    /// Number of requests awaiting a response.
    pub fn outstanding_requests(&self) -> usize {
        self.outstanding.len()
    }

    /// Whether `channel_id` is open (not closed, not in its grace period).
    pub fn is_channel_open(&self, channel_id: u32, now: Instant) -> bool {
        self.channel_state(channel_id, now) == IdState::Open
    }

    /// Whether `subscription_id` is open.
    pub fn is_subscription_open(&self, subscription_id: u32, now: Instant) -> bool {
        self.subscription_state(subscription_id, now) == IdState::Open
    }

    /// The decompressed bins of the last accepted `WaterfallRow` (exactly
    /// `WaterfallSubscribed.bins` bytes). Only meaningful right after
    /// [`check`](Self::check) accepted a row.
    pub fn last_waterfall_row(&self) -> &[u8] {
        &self.row
    }

    /// §2.1: whether `Hello` has not arrived within
    /// [`limits::HELLO_TIMEOUT`] of the stream opening (close the stream).
    pub fn hello_timed_out(&self, now: Instant) -> bool {
        self.version.is_none()
            && now.saturating_duration_since(self.started_at) >= limits::HELLO_TIMEOUT
    }

    /// §2.1: whether nothing at all arrived for [`limits::IDLE_TIMEOUT`]
    /// (close the stream; the station reconnects).
    pub fn idle_timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_message_at) >= limits::IDLE_TIMEOUT
    }

    /// The earliest instant at which a timer of this session fires (hello,
    /// idle or a response deadline).
    pub fn next_deadline(&self) -> Instant {
        let idle = self
            .last_message_at
            .checked_add(limits::IDLE_TIMEOUT)
            .unwrap_or(self.last_message_at);
        let mut next = idle;
        if self.version.is_none() {
            let hello = self
                .started_at
                .checked_add(limits::HELLO_TIMEOUT)
                .unwrap_or(self.started_at);
            next = next.min(hello);
        }
        if let Some(d) = self.outstanding.values().map(|o| o.deadline).min() {
            next = next.min(d);
        }
        next
    }

    /// §2.1: removes the requests whose deadline (5 s, `GetCapabilities`
    /// 10 s) has passed. A late response to one of them is later dropped,
    /// not treated as unsolicited. Three consecutive timeouts (no in-time
    /// response in between) are a violation.
    pub fn expire_requests(&mut self, now: Instant) -> ExpiredRequests {
        let expired: Vec<u64> = self
            .outstanding
            .iter()
            .filter(|(_, o)| o.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        let mut violation = None;
        for id in &expired {
            self.outstanding.remove(id);
            self.timed_out.push_back(*id);
            if self.timed_out.len() > limits::TIMED_OUT_REQUESTS_REMEMBERED {
                self.timed_out.pop_front();
            }
            self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
            if self.consecutive_timeouts >= limits::MAX_CONSECUTIVE_RESPONSE_TIMEOUTS {
                violation = Some(ViolationKind::ResponseTimeouts {
                    consecutive: self.consecutive_timeouts,
                });
            }
        }
        ExpiredRequests {
            request_ids: expired,
            violation,
        }
    }

    /// Checks one station message against §5 and updates the session.
    pub fn check(&mut self, msg: &v1::StationMessage, now: Instant) -> Verdict {
        match self.check_inner(msg, now) {
            Ok(None) => Verdict::Accept,
            Ok(Some(reason)) => Verdict::Drop(reason),
            Err(kind) => Verdict::Violation(kind),
        }
    }

    fn check_inner(
        &mut self,
        msg: &v1::StationMessage,
        now: Instant,
    ) -> Result<Option<DropReason>, ViolationKind> {
        use v1::station_message::Body;
        if now > self.last_message_at {
            self.last_message_at = now;
        }
        let len = msg.encoded_len();
        if len > self.max_message_bytes {
            return Err(ViolationKind::MessageTooLarge {
                len,
                max: self.max_message_bytes,
            });
        }
        let body = msg.body.as_ref().ok_or(ViolationKind::EmptyMessage)?;
        let Some(version) = self.version else {
            let Body::Hello(hello) = body else {
                return Err(ViolationKind::FirstMessageNotHello {
                    got: MessageKind::of(body),
                });
            };
            check_message(msg, crate::PROTOCOL_VERSION.minor)?;
            let negotiated = negotiate(hello).map_err(|e| {
                let peer = e.peer.unwrap_or_default();
                ViolationKind::UnsupportedVersion {
                    major: peer.major,
                    minor: peer.minor,
                }
            })?;
            self.version = Some(negotiated);
            return Ok(None);
        };
        if matches!(body, Body::Hello(_)) {
            return Err(ViolationKind::DuplicateHello);
        }
        check_message(msg, version.minor)?;
        match body {
            Body::Hello(_) => Err(ViolationKind::DuplicateHello),
            Body::Response(r) => self.check_response(r, now),
            Body::Audio(a) => self.check_audio(a, now),
            Body::Waterfall(w) => self.check_waterfall(w, now),
            Body::Signal(s) => self.check_signal(s, now),
            Body::Status(_) => Ok(if self.status.try_take(now) {
                None
            } else {
                Some(DropReason::RateLimited {
                    kind: MessageKind::Status,
                })
            }),
            Body::Event(e) => Ok(self.check_event(e, now)),
        }
    }

    fn check_response(
        &mut self,
        r: &v1::Response,
        now: Instant,
    ) -> Result<Option<DropReason>, ViolationKind> {
        use v1::response::Payload;
        let request_id = r.request_id;
        let Some(out) = self.outstanding.remove(&request_id) else {
            if let Some(pos) = self.timed_out.iter().position(|id| *id == request_id) {
                self.timed_out.remove(pos);
                return Ok(Some(DropReason::LateResponse { request_id }));
            }
            return Err(ViolationKind::UnsolicitedResponse { request_id });
        };
        self.consecutive_timeouts = 0;
        let command = out.pending.kind();
        // `check_message` guaranteed a result with a known code.
        let code = r.result.as_ref().map_or(0, |res| res.code);
        let ok = code == v1::ResultCode::Ok as i32;
        let payload = r.payload.as_ref().map(PayloadKind::of);
        let expected = if ok { command.ok_payload() } else { None };
        if payload != expected {
            return Err(ViolationKind::PayloadMismatch {
                request_id,
                command,
                code,
                payload,
            });
        }
        let mismatch = |field| ViolationKind::ResponseFieldMismatch {
            request_id,
            command,
            field,
        };
        match (out.pending, &r.payload) {
            (Pending::OpenChannel { channel_id }, Some(Payload::ChannelOpened(c))) => {
                if c.channel_id != channel_id {
                    return Err(mismatch("channel_opened.channel_id"));
                }
                let fd = c.audio.as_ref().map_or(0, |a| a.frame_duration_ms);
                if let Some(ch) = self.channels.get_mut(&channel_id) {
                    ch.audio = audio_bucket(fd, now);
                }
            }
            (Pending::OpenChannel { channel_id }, None) => {
                // Not OK: the station has no such channel.
                self.channels.remove(&channel_id);
            }
            (Pending::UpdateChannel { channel_id }, Some(Payload::ChannelUpdated(c))) => {
                if c.channel_id != channel_id {
                    return Err(mismatch("channel_updated.channel_id"));
                }
            }
            (
                Pending::SubscribeWaterfall {
                    subscription_id,
                    bins,
                    row_interval_ms,
                },
                Some(Payload::WaterfallSubscribed(w)),
            ) => {
                if w.subscription_id != subscription_id {
                    return Err(mismatch("waterfall_subscribed.subscription_id"));
                }
                // The station may give less than asked, never more.
                if w.bins > bins {
                    return Err(mismatch("waterfall_subscribed.bins"));
                }
                if w.row_interval_ms < row_interval_ms {
                    return Err(mismatch("waterfall_subscribed.row_interval_ms"));
                }
                if let Some(sub) = self.subscriptions.get_mut(&subscription_id) {
                    let per_sec =
                        limits::MEDIA_RATE_TOLERANCE * 1000.0 / f64::from(w.row_interval_ms.max(1));
                    sub.params = Some(RowParams {
                        bins: w.bins,
                        compression: w.compression,
                    });
                    sub.rows = Some(TokenBucket::new(per_sec, media_capacity(per_sec), now));
                }
            }
            (
                Pending::SubscribeWaterfall {
                    subscription_id, ..
                },
                None,
            ) => {
                self.subscriptions.remove(&subscription_id);
            }
            (Pending::Ping { nonce }, Some(Payload::Pong(p))) if p.nonce != nonce => {
                return Err(mismatch("pong.nonce"));
            }
            _ => {}
        }
        Ok(None)
    }

    fn check_audio(
        &mut self,
        a: &v1::AudioFrame,
        now: Instant,
    ) -> Result<Option<DropReason>, ViolationKind> {
        let kind = MessageKind::Audio;
        match self.channel_state(a.channel_id, now) {
            IdState::Unknown => Err(ViolationKind::UnknownChannel {
                kind,
                channel_id: a.channel_id,
            }),
            IdState::Grace => Ok(Some(DropReason::RecentlyClosed {
                kind,
                id: a.channel_id,
            })),
            IdState::Open => {
                let Some(ch) = self.channels.get_mut(&a.channel_id) else {
                    return Err(ViolationKind::UnknownChannel {
                        kind,
                        channel_id: a.channel_id,
                    });
                };
                if ch.audio.try_take(now) {
                    return Ok(None);
                }
                if ch.audio_excess.on_excess(
                    now,
                    Duration::ZERO,
                    limits::SUSTAINED_GAP,
                    limits::AUDIO_RATE_SUSTAINED,
                ) {
                    Err(ViolationKind::AudioRateSustained {
                        channel_id: a.channel_id,
                    })
                } else {
                    Ok(Some(DropReason::RateLimited { kind }))
                }
            }
        }
    }

    fn check_waterfall(
        &mut self,
        w: &v1::WaterfallRow,
        now: Instant,
    ) -> Result<Option<DropReason>, ViolationKind> {
        let id = w.subscription_id;
        match self.subscription_state(id, now) {
            IdState::Unknown => Err(ViolationKind::UnknownSubscription {
                subscription_id: id,
            }),
            IdState::Grace => Ok(Some(DropReason::RecentlyClosed {
                kind: MessageKind::Waterfall,
                id,
            })),
            IdState::Open => {
                let Some(sub) = self.subscriptions.get_mut(&id) else {
                    return Err(ViolationKind::UnknownSubscription {
                        subscription_id: id,
                    });
                };
                let (Some(params), Some(rows)) = (sub.params, sub.rows.as_mut()) else {
                    return Ok(Some(DropReason::AwaitingSubscription {
                        subscription_id: id,
                    }));
                };
                if !rows.try_take(now) {
                    return Err(ViolationKind::WaterfallRate {
                        subscription_id: id,
                    });
                }
                self.decoder
                    .decode(&w.payload, params.compression, params.bins, &mut self.row)?;
                Ok(None)
            }
        }
    }

    fn check_signal(
        &mut self,
        s: &v1::ChannelSignal,
        now: Instant,
    ) -> Result<Option<DropReason>, ViolationKind> {
        let kind = MessageKind::Signal;
        match self.channel_state(s.channel_id, now) {
            IdState::Unknown => Err(ViolationKind::UnknownChannel {
                kind,
                channel_id: s.channel_id,
            }),
            IdState::Grace => Ok(Some(DropReason::RecentlyClosed {
                kind,
                id: s.channel_id,
            })),
            IdState::Open => {
                let taken = self
                    .channels
                    .get_mut(&s.channel_id)
                    .is_some_and(|ch| ch.signal.try_take(now));
                Ok((!taken).then_some(DropReason::RateLimited { kind }))
            }
        }
    }

    fn check_event(&mut self, e: &v1::Event, now: Instant) -> Option<DropReason> {
        use v1::event::Kind;
        if !self.events.try_take(now) {
            return Some(DropReason::RateLimited {
                kind: MessageKind::Event,
            });
        }
        if let Some(id) = e.channel_id {
            if self.channel_state(id, now) == IdState::Unknown {
                return Some(DropReason::UnknownEventTarget);
            }
        }
        if let Some(id) = e.subscription_id {
            if self.subscription_state(id, now) == IdState::Unknown {
                return Some(DropReason::UnknownEventTarget);
            }
        }
        match Kind::try_from(e.kind) {
            Ok(Kind::ChannelClosed) => {
                if let Some(ch) = e.channel_id.and_then(|id| self.channels.get_mut(&id)) {
                    ch.closed_at.get_or_insert(now);
                }
            }
            Ok(Kind::SubscriptionClosed) => {
                if let Some(sub) = e
                    .subscription_id
                    .and_then(|id| self.subscriptions.get_mut(&id))
                {
                    sub.closed_at.get_or_insert(now);
                }
            }
            Ok(Kind::BackendRestarted) => {
                for ch in self.channels.values_mut() {
                    ch.closed_at.get_or_insert(now);
                }
                for sub in self.subscriptions.values_mut() {
                    sub.closed_at.get_or_insert(now);
                }
            }
            _ => {}
        }
        None
    }

    fn channel_state(&self, id: u32, now: Instant) -> IdState {
        self.channels
            .get(&id)
            .map_or(IdState::Unknown, |c| id_state(c.closed_at, now))
    }

    fn subscription_state(&self, id: u32, now: Instant) -> IdState {
        self.subscriptions
            .get(&id)
            .map_or(IdState::Unknown, |s| id_state(s.closed_at, now))
    }

    /// Forgets IDs whose grace period is over.
    fn collect_garbage(&mut self, now: Instant) {
        self.channels
            .retain(|_, c| id_state(c.closed_at, now) != IdState::Unknown);
        self.subscriptions
            .retain(|_, s| id_state(s.closed_at, now) != IdState::Unknown);
    }
}

/// §5 `StationStatus`: at most one per `status_interval / 2`.
fn status_bucket(interval: Duration, now: Instant) -> TokenBucket {
    let secs = interval.as_secs_f64().max(0.001);
    TokenBucket::new(f64::from(limits::STATUS_RATE_DIVISOR) / secs, 1.0, now)
}
