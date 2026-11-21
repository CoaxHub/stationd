// SPDX-License-Identifier: GPL-3.0-only
//! Answers hub requests (station-protocol §3, §4): every `Request` gets
//! exactly one `Response`.
//!
//! | Command | Answer |
//! |---|---|
//! | unknown command or enum value (newer minor) | `UNSUPPORTED` (§3) |
//! | missing required field, `UNSPECIFIED`, `request_id` 0 | `INVALID_ARGUMENT` |
//! | `GetHardware` | the SDRs plugged in, with the table's models and profiles ([`crate::hardware`]) |
//! | `ConfigureReceivers` | the set checked, applied and answered ([`Receivers::configure`]), or refused with nothing changed |
//! | `GetCapabilities` | the running receivers and the station-wide limits ([`Receivers::capabilities`]) |
//! | `Ping` | `Pong` with the same nonce |
//! | `AdminOp` | `UNSUPPORTED`: this station implements no admin operation |
//! | `SetFrontend` | `NOT_FOUND` unless the receiver runs; a new centre checked against its profile and the other receivers; then its backend |
//! | `OpenChannel` / `SubscribeWaterfall` | `NOT_FOUND` unless the receiver runs; `ALREADY_EXISTS`; `RESOURCE_EXHAUSTED` beyond the receiver's channels or the station's subscriptions (§7); then the receiver's backend |
//! | `UpdateChannel` / `CloseChannel` / `UnsubscribeWaterfall` | `NOT_FOUND` unless open (or opening, for a close) on this connection, then the backend of its receiver |
//! | `StartMeasurements` | `UNSUPPORTED` without measuring, `ALREADY_EXISTS` while a plan runs, else the plan as it will run ([`crate::measure`]) |
//! | `StopMeasurements` | `OK`, or `NOT_FOUND` for a plan that isn't running |
//!
//! An accepted `SetFrontend` stops the running plan first
//! (`Event{MEASUREMENT_STOPPED}`): a measurement never straddles a change of
//! settings.
//!
//! Channels and subscriptions belong to one connection ([`Tables`], §2.3),
//! each on the receiver it was opened on (§4).
//!
//! # Order
//!
//! Requests are answered concurrently, but their effect on the tables is
//! not: [`admit`] runs in the session's read loop, in the order the hub
//! sent the requests, and does the table step of each one before the next
//! is read. An `OpenChannel`/`SubscribeWaterfall` is checked and reserves
//! its ID and its place against the limits; a `CloseChannel`/
//! `UnsubscribeWaterfall` frees its place at once, whether the ID is open
//! or still opening. What is left (the backend call and the answer) is
//! [`finish`]. A new open of an ID that is still being closed in its
//! backend waits for that close before it calls the backend, so one ID's
//! backend calls never overlap or swap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use station_backend::{AudioStream, Backend, BackendError, RowStream};
use station_proto::validate::check_request;
use station_proto::{limits, v1};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::measure::{Measurer, StopReason};
use crate::receivers::Receivers;

use v1::request::Command;
use v1::response::Payload;

/// The lifecycle of a channel or subscription ID on one connection.
#[derive(Debug)]
enum Slot {
    /// The backend call is in flight. `close_requested`: the hub closed it
    /// before the open finished; it no longer counts toward any limit and
    /// is closed in its backend as soon as the open ends. `ticket` names
    /// this open (an ID can be opened again while an old open is still
    /// finishing); `done` is cancelled when the open has ended and its
    /// backend state is settled.
    Opening {
        ticket: u64,
        done: CancellationToken,
        close_requested: bool,
    },
    /// Open; cancelling `stop` ends the media pump.
    Open { stop: CancellationToken },
    /// Closed by the hub; the backend close is in flight. Counts toward no
    /// limit. `done` is cancelled when it has ended.
    Closing {
        ticket: u64,
        done: CancellationToken,
    },
}

/// A channel or subscription, on its receiver.
#[derive(Debug)]
struct Entry {
    receiver_id: u32,
    slot: Slot,
}

impl Entry {
    #[cfg(test)]
    fn opening(receiver_id: u32) -> Self {
        Self {
            receiver_id,
            slot: Slot::Opening {
                ticket: 0,
                done: CancellationToken::new(),
                close_requested: false,
            },
        }
    }

    fn is_open(&self) -> bool {
        matches!(self.slot, Slot::Open { .. })
    }

    /// Open or opening, and not closed by the hub: what the limits count.
    fn is_live(&self) -> bool {
        matches!(
            self.slot,
            Slot::Open { .. }
                | Slot::Opening {
                    close_requested: false,
                    ..
                }
        )
    }

    /// Closed by the hub, its backend state not settled yet: the token to
    /// wait on before reusing the ID.
    fn winding_down(&self) -> Option<&CancellationToken> {
        match &self.slot {
            Slot::Opening {
                done,
                close_requested: true,
                ..
            }
            | Slot::Closing { done, .. } => Some(done),
            _ => None,
        }
    }

    fn stop(self) {
        if let Slot::Open { stop } = self.slot {
            stop.cancel();
        }
    }

    /// The backend ended this entry by itself. An open one is stopped, and
    /// `true` says to forget it; an opening one is marked close-requested (its
    /// open closes it in the backend when it ends); a closing one is left,
    /// its close is still in flight.
    fn backend_ended(&mut self) -> bool {
        match &mut self.slot {
            Slot::Open { stop } => {
                stop.cancel();
                true
            }
            Slot::Opening {
                close_requested, ..
            } => {
                *close_requested = true;
                false
            }
            Slot::Closing { .. } => false,
        }
    }
}

/// [`Entry::backend_ended`] for `id` of `map`.
fn backend_ended(map: &mut HashMap<u32, Entry>, id: u32) {
    if map.get_mut(&id).is_some_and(Entry::backend_ended) {
        map.remove(&id);
    }
}

/// The channels and subscriptions of one connection.
#[derive(Debug, Default)]
pub struct Tables {
    channels: HashMap<u32, Entry>,
    subscriptions: HashMap<u32, Entry>,
    next_ticket: u64,
}

/// IDs with their receivers: `(receiver_id, id)`, sorted.
pub type Owned = Vec<(u32, u32)>;

impl Tables {
    /// Open channels (not counting ones being opened).
    pub fn open_channels(&self) -> usize {
        self.channels.values().filter(|e| e.is_open()).count()
    }

    /// Open subscriptions (not counting ones being opened).
    pub fn open_subscriptions(&self) -> usize {
        self.subscriptions.values().filter(|e| e.is_open()).count()
    }

    /// Open channels on `receiver_id`.
    pub fn open_channels_on(&self, receiver_id: u32) -> usize {
        self.channels
            .values()
            .filter(|e| e.receiver_id == receiver_id && e.is_open())
            .count()
    }

    /// Open listener channels and subscriptions on `receiver_id` (its duty,
    /// §4.1).
    pub fn listeners_on(&self, receiver_id: u32) -> usize {
        self.open_channels_on(receiver_id)
            + self
                .subscriptions
                .values()
                .filter(|e| e.receiver_id == receiver_id && e.is_open())
                .count()
    }

    /// Open channels per receiver, for the status (so no table lock is
    /// taken while the receivers' lock is held).
    pub fn open_channels_by_receiver(&self) -> HashMap<u32, u32> {
        let mut counts = HashMap::new();
        for e in self.channels.values().filter(|e| e.is_open()) {
            *counts.entry(e.receiver_id).or_insert(0u32) += 1;
        }
        counts
    }

    /// A channel the backend closed by itself: an open one is forgotten,
    /// its pump stopped; one still opening is marked closed (it closes in
    /// its backend when its open ends); one the hub is closing is left to
    /// its close.
    pub fn channel_closed(&mut self, id: u32) {
        backend_ended(&mut self.channels, id);
    }

    /// A subscription the backend closed by itself, as
    /// [`channel_closed`](Self::channel_closed).
    pub fn subscription_closed(&mut self, id: u32) {
        backend_ended(&mut self.subscriptions, id);
    }

    /// Everything on `receiver_id`, whose backend is gone
    /// (`BACKEND_RESTARTED`), as [`channel_closed`](Self::channel_closed):
    /// the open ones are forgotten and their pumps stopped; an opening or
    /// closing entry stays until its own backend call ends.
    pub fn receiver_gone(&mut self, receiver_id: u32) {
        for map in [&mut self.channels, &mut self.subscriptions] {
            let gone: Vec<u32> = map
                .iter()
                .filter(|(_, e)| e.receiver_id == receiver_id)
                .map(|(id, _)| *id)
                .collect();
            for id in gone {
                backend_ended(map, id);
            }
        }
    }

    /// Empties both tables, stopping every pump. Returns the channels and
    /// subscriptions that were known (open or opening), for closing them in
    /// their backends.
    pub fn take_all(&mut self) -> (Owned, Owned) {
        let stop_all = |m: &mut HashMap<u32, Entry>| {
            let mut ids: Owned = m
                .drain()
                .map(|(id, e)| {
                    let receiver_id = e.receiver_id;
                    e.stop();
                    (receiver_id, id)
                })
                .collect();
            ids.sort_unstable();
            ids
        };
        (
            stop_all(&mut self.channels),
            stop_all(&mut self.subscriptions),
        )
    }
}

/// What a request needs from its connection.
#[derive(Clone)]
pub struct RequestContext {
    /// The station's receivers.
    pub receivers: Receivers,
    /// The negotiated protocol minor (§3).
    pub minor: u32,
    /// The connection's channel and subscription tables.
    pub tables: Arc<Mutex<Tables>>,
    /// The connection's measurement plan; `None`: the station doesn't
    /// measure on this connection.
    pub measurer: Option<Arc<Measurer>>,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("minor", &self.minor)
            .finish_non_exhaustive()
    }
}

impl RequestContext {
    fn tables(&self) -> MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Media to pump after the response is queued.
pub enum Media {
    /// Audio of a newly opened channel.
    Audio {
        /// The channel ID.
        channel_id: u32,
        /// The backend's stream.
        stream: AudioStream,
        /// Cancelled when the channel is closed.
        stop: CancellationToken,
    },
    /// Rows of a new subscription.
    Rows {
        /// The subscription ID.
        subscription_id: u32,
        /// The backend's stream.
        stream: RowStream,
        /// Cancelled when the subscription is closed.
        stop: CancellationToken,
    },
}

impl std::fmt::Debug for Media {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Audio { channel_id, .. } => f
                .debug_struct("Audio")
                .field("channel_id", channel_id)
                .finish_non_exhaustive(),
            Self::Rows {
                subscription_id, ..
            } => f
                .debug_struct("Rows")
                .field("subscription_id", subscription_id)
                .finish_non_exhaustive(),
        }
    }
}

/// Truncates `s` to at most `max` bytes on a character boundary.
pub fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn result(code: v1::ResultCode, message: &str) -> v1::Result {
    v1::Result {
        code: code as i32,
        message: truncate_utf8(message, limits::MAX_DETAIL_BYTES),
    }
}

fn ok(request_id: u64, payload: Option<Payload>) -> v1::Response {
    v1::Response {
        request_id,
        result: Some(result(v1::ResultCode::Ok, "")),
        payload,
    }
}

fn error(request_id: u64, code: v1::ResultCode, message: &str) -> v1::Response {
    v1::Response {
        request_id,
        result: Some(result(code, message)),
        payload: None,
    }
}

fn backend_error(request_id: u64, e: &BackendError) -> v1::Response {
    // A backend must never answer OK/UNSPECIFIED as an error; map defensively.
    let code = match e.code {
        v1::ResultCode::Ok | v1::ResultCode::Unspecified => v1::ResultCode::BackendError,
        c => c,
    };
    error(request_id, code, &e.message)
}

fn not_running(request_id: u64, receiver_id: u32) -> v1::Response {
    error(
        request_id,
        v1::ResultCode::NotFound,
        &format!("receiver {receiver_id} is not running"),
    )
}

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A request after its step on the connection's tables, taken in the order
/// the hub sent it ([`admit`]); [`finish`] does the rest.
pub struct Admitted(Step);

impl std::fmt::Debug for Admitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admitted").finish_non_exhaustive()
    }
}

enum Step {
    /// Already answered (rejected, refused, or closed in the tables only).
    Answered(v1::Response),
    /// A request with no table step.
    Later(u64, Command),
    /// A channel reserved in the tables: open it in its backend.
    OpenChannel(u64, v1::OpenChannel, Arc<dyn Backend>, Pending),
    /// A subscription reserved in the tables: open it in its backend.
    Subscribe(u64, v1::SubscribeWaterfall, Arc<dyn Backend>, Pending),
    /// A channel removed from the tables: close it in its backend (`None`:
    /// its receiver is gone).
    CloseChannel(u64, u32, Option<Arc<dyn Backend>>, Pending),
    /// A subscription removed from the tables: close it in its backend.
    Unsubscribe(u64, u32, Option<Arc<dyn Backend>>, Pending),
}

/// The table step of one open or close, held until its backend calls end.
/// Dropping it tells whoever waits to reuse the ID that the ID is settled.
#[derive(Debug)]
struct Pending {
    ticket: u64,
    /// The earlier open or close of the same ID still in flight.
    same_id: Option<CancellationToken>,
    /// The earlier closes still in flight on its receiver: wait for them
    /// before calling the backend, so the room a client freed is free in
    /// the backend when it takes it.
    others: Vec<CancellationToken>,
    _done: tokio_util::sync::DropGuard,
}

/// How long an open waits for the closes before it.
const WAIT_LIMIT: Duration = Duration::from_secs(3);

impl Pending {
    /// Waits (at most [`WAIT_LIMIT`]) for what is before this one. `false`:
    /// the time ran out while the same ID was still being closed, so the
    /// open must not go on. When only other IDs are late, it goes ahead and
    /// the backend answers, `RESOURCE_EXHAUSTED` if it has no room.
    async fn after_previous(&self) -> bool {
        let all = async {
            for w in self.same_id.iter().chain(&self.others) {
                w.cancelled().await;
            }
        };
        if tokio::time::timeout(WAIT_LIMIT, all).await.is_ok() {
            return true;
        }
        self.same_id
            .as_ref()
            .is_none_or(CancellationToken::is_cancelled)
    }
}

/// What an open answers when it cannot go on after the wait.
fn still_closing(id: u64, what: &str, target: u32) -> v1::Response {
    error(
        id,
        v1::ResultCode::BackendError,
        &format!("the earlier close of {what} {target} did not finish in time"),
    )
}

/// Answers `req`. Always returns exactly one response for `req.request_id`,
/// plus media to pump when a channel or subscription was opened.
pub async fn dispatch(ctx: &RequestContext, req: v1::Request) -> (v1::Response, Option<Media>) {
    finish(ctx, admit(ctx, req)).await
}

/// The first half of [`dispatch`], without waiting: validates `req` and does
/// its step on the tables. The session calls it in its read loop, one
/// request after the other, so the tables change in the hub's order
/// whatever order the tasks of [`finish`] run in.
pub fn admit(ctx: &RequestContext, req: v1::Request) -> Admitted {
    let id = req.request_id;
    if let Err(rejection) = check_request(&req, ctx.minor) {
        return Admitted(Step::Answered(v1::Response {
            request_id: id,
            result: Some(rejection.to_result()),
            payload: None,
        }));
    }
    let Some(command) = req.command else {
        // check_request rejects a missing command; unreachable in practice.
        return Admitted(Step::Answered(error(
            id,
            v1::ResultCode::Unsupported,
            "no command",
        )));
    };
    Admitted(match command {
        Command::OpenChannel(o) => admit_open_channel(ctx, id, o),
        Command::CloseChannel(c) => admit_close(ctx, id, c.channel_id, true),
        Command::SubscribeWaterfall(s) => admit_subscribe(ctx, id, s),
        Command::UnsubscribeWaterfall(u) => admit_close(ctx, id, u.subscription_id, false),
        other => Step::Later(id, other),
    })
}

/// The second half of [`dispatch`]: the backend calls and the answer.
pub async fn finish(ctx: &RequestContext, admitted: Admitted) -> (v1::Response, Option<Media>) {
    let (response, media) = match admitted.0 {
        Step::Answered(response) => (response, None),
        Step::Later(id, command) => answer(ctx, id, command).await,
        Step::OpenChannel(id, req, backend, pending) => {
            open_channel(ctx, id, req, &*backend, pending).await
        }
        Step::Subscribe(id, req, backend, pending) => {
            subscribe(ctx, id, req, &*backend, pending).await
        }
        Step::CloseChannel(id, channel_id, backend, pending) => (
            close_channel(ctx, id, channel_id, backend, pending).await,
            None,
        ),
        Step::Unsubscribe(id, subscription_id, backend, pending) => (
            unsubscribe(ctx, id, subscription_id, backend, pending).await,
            None,
        ),
    };
    tracing::debug!(
        request_id = response.request_id,
        code = response.result.as_ref().map_or(0, |r| r.code),
        "answered request"
    );
    (response, media)
}

async fn answer(ctx: &RequestContext, id: u64, command: Command) -> (v1::Response, Option<Media>) {
    match command {
        Command::GetHardware(_) => (
            ok(id, Some(Payload::Hardware(ctx.receivers.hardware()))),
            None,
        ),
        Command::ConfigureReceivers(set) => (configure(ctx, id, &set).await, None),
        Command::GetCapabilities(_) => (get_capabilities(ctx, id).await, None),
        Command::Ping(p) => {
            let pong = v1::Pong {
                nonce: p.nonce,
                station_time_unix_ms: wall_clock_ms(),
            };
            (ok(id, Some(Payload::Pong(pong))), None)
        }
        Command::AdminOp(op) => (admin_op(id, op.kind), None),
        Command::SetFrontend(f) => (set_frontend(ctx, id, f).await, None),
        Command::UpdateChannel(u) => (update_channel(ctx, id, u).await, None),
        Command::StartMeasurements(m) => (start_measurements(ctx, id, m).await, None),
        Command::StopMeasurements(m) => (stop_measurements(ctx, id, m.measurement_id), None),
        // Their table step is `admit`'s.
        Command::OpenChannel(_)
        | Command::CloseChannel(_)
        | Command::SubscribeWaterfall(_)
        | Command::UnsubscribeWaterfall(_) => (
            error(id, v1::ResultCode::BackendError, "request not admitted"),
            None,
        ),
    }
}

async fn configure(ctx: &RequestContext, id: u64, set: &v1::ConfigureReceivers) -> v1::Response {
    match ctx.receivers.configure(set).await {
        Ok(configured) => ok(id, Some(Payload::ReceiversConfigured(configured))),
        Err(refusal) => v1::Response {
            request_id: id,
            result: Some(result(
                v1::ResultCode::try_from(refusal.code).unwrap_or(v1::ResultCode::BackendError),
                &refusal.message,
            )),
            payload: None,
        },
    }
}

async fn start_measurements(
    ctx: &RequestContext,
    id: u64,
    req: v1::StartMeasurements,
) -> v1::Response {
    let Some(measurer) = &ctx.measurer else {
        return error(
            id,
            v1::ResultCode::Unsupported,
            "this station does not measure",
        );
    };
    match measurer.start(req).await {
        Ok(started) => ok(id, Some(Payload::MeasurementsStarted(started))),
        Err(refusal) => error(id, refusal.code, &refusal.message),
    }
}

fn stop_measurements(ctx: &RequestContext, id: u64, measurement_id: u32) -> v1::Response {
    if ctx
        .measurer
        .as_ref()
        .is_some_and(|m| m.stop(measurement_id))
    {
        ok(id, None)
    } else {
        error(
            id,
            v1::ResultCode::NotFound,
            &format!("measurement {measurement_id} is not running"),
        )
    }
}

async fn get_capabilities(ctx: &RequestContext, id: u64) -> v1::Response {
    let mut caps = ctx.receivers.capabilities().await;
    if ctx.measurer.is_none() {
        caps.measurements = None;
    }
    ok(id, Some(Payload::Capabilities(caps)))
}

fn admin_op(id: u64, kind: i32) -> v1::Response {
    let name = v1::AdminOpKind::try_from(kind).map_or("unknown", |k| k.as_str_name());
    info!(
        admin_op = name,
        "admin operation refused: not implemented by this station"
    );
    error(
        id,
        v1::ResultCode::Unsupported,
        "this station implements no admin operation",
    )
}

async fn set_frontend(ctx: &RequestContext, id: u64, req: v1::SetFrontend) -> v1::Response {
    let receiver_id = req.receiver_id;
    let Some(backend) = ctx.receivers.running_backend(receiver_id) else {
        return not_running(id, receiver_id);
    };
    let center = req.center_frequency_hz;
    if let Some(hz) = center {
        if let Err(e) = ctx.receivers.check_retune(receiver_id, hz) {
            return backend_error(id, &e);
        }
    }
    match backend.set_frontend(req).await {
        Ok(mut state) => {
            state.receiver_id = receiver_id;
            if let Some(m) = &ctx.measurer {
                m.stop_for(StopReason::FrontendChanged);
            }
            if let Some(hz) = center {
                ctx.receivers.retuned(receiver_id, hz).await;
            }
            ok(id, Some(Payload::FrontendState(state)))
        }
        Err(e) => backend_error(id, &e),
    }
}

/// The table step of an open on `map`: the ID is free (or still winding
/// down from a close; the open waits for the closes before it). The caller has
/// checked the ID is not in use and the limits.
fn reserve(map: &mut HashMap<u32, Entry>, next_ticket: &mut u64, id: u32, rx: u32) -> Pending {
    let same_id = map.get(&id).and_then(|e| e.winding_down().cloned());
    let others = map
        .iter()
        .filter(|(i, e)| **i != id && e.receiver_id == rx)
        .filter_map(|(_, e)| e.winding_down().cloned())
        .collect();
    *next_ticket += 1;
    let ticket = *next_ticket;
    let done = CancellationToken::new();
    map.insert(
        id,
        Entry {
            receiver_id: rx,
            slot: Slot::Opening {
                ticket,
                done: done.clone(),
                close_requested: false,
            },
        },
    );
    Pending {
        ticket,
        same_id,
        others,
        _done: done.drop_guard(),
    }
}

/// An ID that is still winding down (closed by the hub, not settled) on a
/// receiver other than `rx`: it cannot be taken by an open on `rx`, whose
/// backend would not wait for the other's close.
fn winding_down_elsewhere(map: &HashMap<u32, Entry>, id: u32, rx: u32) -> Option<u32> {
    map.get(&id)
        .filter(|e| e.receiver_id != rx && e.winding_down().is_some())
        .map(|e| e.receiver_id)
}

/// Whether `id` is open or opening (not closed by the hub).
fn in_use(map: &HashMap<u32, Entry>, id: u32) -> bool {
    map.get(&id).is_some_and(Entry::is_live)
}

fn admit_open_channel(ctx: &RequestContext, id: u64, req: v1::OpenChannel) -> Step {
    let (channel_id, receiver_id) = (req.channel_id, req.receiver_id);
    let Some(backend) = ctx.receivers.running_backend(receiver_id) else {
        return Step::Answered(not_running(id, receiver_id));
    };
    let mut t = ctx.tables();
    let Tables {
        channels,
        next_ticket,
        ..
    } = &mut *t;
    if in_use(channels, channel_id) {
        return Step::Answered(error(
            id,
            v1::ResultCode::AlreadyExists,
            &format!("channel {channel_id} is already open"),
        ));
    }
    if let Some(other) = winding_down_elsewhere(channels, channel_id, receiver_id) {
        return Step::Answered(error(
            id,
            v1::ResultCode::AlreadyExists,
            &format!("channel {channel_id} is still being closed on receiver {other}"),
        ));
    }
    let max = usize::try_from(limits::CHANNELS_PER_RECEIVER).unwrap_or(usize::MAX);
    let on_receiver = channels
        .values()
        .filter(|e| e.receiver_id == receiver_id && e.is_live())
        .count();
    if on_receiver >= max {
        return Step::Answered(error(
            id,
            v1::ResultCode::ResourceExhausted,
            &format!(
                "receiver {receiver_id} has all its {} channels in use",
                limits::CHANNELS_PER_RECEIVER
            ),
        ));
    }
    // Open and close pairs cannot pile up without bound: what is winding
    // down counts too, up to as many again.
    let known = channels
        .iter()
        .filter(|(i, e)| **i != channel_id && e.receiver_id == receiver_id)
        .count();
    if known >= max.saturating_mul(2) {
        return Step::Answered(error(
            id,
            v1::ResultCode::ResourceExhausted,
            &format!("receiver {receiver_id} has too many channels still being closed"),
        ));
    }
    let pending = reserve(channels, next_ticket, channel_id, receiver_id);
    Step::OpenChannel(id, req, backend, pending)
}

async fn open_channel(
    ctx: &RequestContext,
    id: u64,
    req: v1::OpenChannel,
    backend: &dyn Backend,
    pending: Pending,
) -> (v1::Response, Option<Media>) {
    let channel_id = req.channel_id;
    let receiver_id = req.receiver_id;
    if !pending.after_previous().await {
        release(&mut ctx.tables().channels, channel_id, pending.ticket);
        return (still_closing(id, "channel", channel_id), None);
    }
    let skip = cancelled(&ctx.tables().channels, channel_id, pending.ticket);
    if skip {
        // Closed while it waited: the backend is not called. Answered as an
        // open closed right after it ended.
        release(&mut ctx.tables().channels, channel_id, pending.ticket);
        let opened = v1::ChannelOpened {
            channel_id,
            receiver_id,
            frequency_hz: req.frequency_hz,
            mode: req.mode,
            low_cut_hz: req.low_cut_hz,
            high_cut_hz: req.high_cut_hz,
            audio: req.audio,
        };
        return (ok(id, Some(Payload::ChannelOpened(opened))), None);
    }
    match backend.open_channel(req).await {
        Err(e) => {
            release(&mut ctx.tables().channels, channel_id, pending.ticket);
            (backend_error(id, &e), None)
        }
        Ok((mut opened, stream)) => {
            opened.channel_id = channel_id;
            opened.receiver_id = receiver_id;
            let stop = CancellationToken::new();
            let keep = promote(
                &mut ctx.tables().channels,
                channel_id,
                pending.ticket,
                &stop,
            );
            let response = ok(id, Some(Payload::ChannelOpened(opened)));
            if keep {
                let media = Media::Audio {
                    channel_id,
                    stream,
                    stop,
                };
                (response, Some(media))
            } else {
                // Closed (or the connection ended) while opening.
                let _ = backend.close_channel(channel_id).await;
                release(&mut ctx.tables().channels, channel_id, pending.ticket);
                (response, None)
            }
        }
    }
}

/// Whether the open `ticket` of `id` is no longer wanted: closed by the hub,
/// or gone from the table (the connection ended).
fn cancelled(map: &HashMap<u32, Entry>, id: u32, ticket: u64) -> bool {
    !matches!(
        map.get(&id),
        Some(Entry {
            slot: Slot::Opening { ticket: t, close_requested: false, .. },
            ..
        }) if *t == ticket
    )
}

/// After a successful open: `Opening` becomes `Open`, unless the hub closed
/// it meanwhile (or the connection ended, or its receiver went, or the ID
/// was taken by a later open); then `false` is returned and the caller
/// closes it in the backend.
fn promote(map: &mut HashMap<u32, Entry>, id: u32, ticket: u64, stop: &CancellationToken) -> bool {
    let Some(entry) = map.get_mut(&id) else {
        return false;
    };
    let mine = matches!(
        entry.slot,
        Slot::Opening { ticket: t, close_requested: false, .. } if t == ticket
    );
    if mine {
        entry.slot = Slot::Open { stop: stop.clone() };
    }
    mine
}

/// Forgets the open or close `ticket` of `id`, whose backend state is
/// settled; an entry of a later open of the same ID is left.
fn release(map: &mut HashMap<u32, Entry>, id: u32, ticket: u64) {
    let mine = matches!(
        map.get(&id),
        Some(Entry {
            slot: Slot::Opening { ticket: t, .. } | Slot::Closing { ticket: t, .. },
            ..
        }) if *t == ticket
    );
    if mine {
        map.remove(&id);
    }
}

/// The receiver of an open channel or subscription.
fn open_on(map: &HashMap<u32, Entry>, id: u32) -> Option<u32> {
    map.get(&id).filter(|e| e.is_open()).map(|e| e.receiver_id)
}

async fn update_channel(ctx: &RequestContext, id: u64, req: v1::UpdateChannel) -> v1::Response {
    let channel_id = req.channel_id;
    // The tables' lock is dropped before the receivers' is taken: the status
    // takes them the other way round.
    let receiver_id = open_on(&ctx.tables().channels, channel_id);
    let backend = receiver_id.and_then(|rx| ctx.receivers.running_backend(rx));
    let Some(backend) = backend else {
        return error(
            id,
            v1::ResultCode::NotFound,
            &format!("channel {channel_id} is not open"),
        );
    };
    match backend.update_channel(req).await {
        Ok(mut updated) => {
            updated.channel_id = channel_id;
            ok(id, Some(Payload::ChannelUpdated(updated)))
        }
        Err(e) => backend_error(id, &e),
    }
}

/// `CloseChannel` / `UnsubscribeWaterfall` on behalf of the hub, taking
/// effect on the tables now: the ID stops counting toward the limits at
/// once. Open: the backend close follows (`finish`). Still opening: the
/// open is cancelled, and closed in its backend as soon as it ends.
fn admit_close(ctx: &RequestContext, id: u64, target: u32, channel: bool) -> Step {
    let what = if channel { "channel" } else { "subscription" };
    let not_open = || {
        Step::Answered(error(
            id,
            v1::ResultCode::NotFound,
            &format!("{what} {target} is not open"),
        ))
    };
    let (receiver_id, pending) = {
        let mut t = ctx.tables();
        let Tables {
            channels,
            subscriptions,
            next_ticket,
        } = &mut *t;
        let map = if channel { channels } else { subscriptions };
        match map.get_mut(&target) {
            None => return not_open(),
            Some(e) if !e.is_live() => return not_open(),
            Some(Entry {
                slot: Slot::Opening {
                    close_requested, ..
                },
                ..
            }) => {
                *close_requested = true;
                return Step::Answered(ok(id, None));
            }
            Some(entry) => {
                *next_ticket += 1;
                let ticket = *next_ticket;
                let done = CancellationToken::new();
                let old = std::mem::replace(
                    &mut entry.slot,
                    Slot::Closing {
                        ticket,
                        done: done.clone(),
                    },
                );
                if let Slot::Open { stop } = old {
                    stop.cancel();
                }
                let pending = Pending {
                    ticket,
                    same_id: None,
                    others: Vec::new(),
                    _done: done.drop_guard(),
                };
                (entry.receiver_id, pending)
            }
        }
    };
    // Like an open, a close takes its backend now, in the hub's order (the
    // tables' lock is already dropped).
    let backend = ctx.receivers.running_backend(receiver_id);
    if channel {
        Step::CloseChannel(id, target, backend, pending)
    } else {
        Step::Unsubscribe(id, target, backend, pending)
    }
}

/// The backend's answer to a close: `NOT_FOUND` there means closed either
/// way.
fn closed(id: u64, r: Result<(), BackendError>) -> v1::Response {
    match r {
        Ok(()) => ok(id, None),
        Err(e) if e.code == v1::ResultCode::NotFound => ok(id, None),
        Err(e) => backend_error(id, &e),
    }
}

async fn close_channel(
    ctx: &RequestContext,
    id: u64,
    channel_id: u32,
    backend: Option<Arc<dyn Backend>>,
    pending: Pending,
) -> v1::Response {
    let response = match backend {
        Some(backend) => closed(id, backend.close_channel(channel_id).await),
        // Its receiver went: the channel went with it.
        None => ok(id, None),
    };
    release(&mut ctx.tables().channels, channel_id, pending.ticket);
    response
}

fn admit_subscribe(ctx: &RequestContext, id: u64, req: v1::SubscribeWaterfall) -> Step {
    let (subscription_id, receiver_id) = (req.subscription_id, req.receiver_id);
    let Some(backend) = ctx.receivers.running_backend(receiver_id) else {
        return Step::Answered(not_running(id, receiver_id));
    };
    let max = limits::waterfall_subscriptions_claim(ctx.receivers.running().len());
    let mut t = ctx.tables();
    let Tables {
        subscriptions,
        next_ticket,
        ..
    } = &mut *t;
    if in_use(subscriptions, subscription_id) {
        return Step::Answered(error(
            id,
            v1::ResultCode::AlreadyExists,
            &format!("subscription {subscription_id} is already open"),
        ));
    }
    if let Some(other) = winding_down_elsewhere(subscriptions, subscription_id, receiver_id) {
        return Step::Answered(error(
            id,
            v1::ResultCode::AlreadyExists,
            &format!("subscription {subscription_id} is still being closed on receiver {other}"),
        ));
    }
    let live = subscriptions.values().filter(|e| e.is_live()).count();
    if live >= usize::try_from(max).unwrap_or(usize::MAX) {
        return Step::Answered(error(
            id,
            v1::ResultCode::ResourceExhausted,
            &format!("all {max} waterfall subscriptions of this station are in use"),
        ));
    }
    let known = subscriptions
        .keys()
        .filter(|i| **i != subscription_id)
        .count();
    if known >= usize::try_from(max).unwrap_or(usize::MAX).saturating_mul(2) {
        return Step::Answered(error(
            id,
            v1::ResultCode::ResourceExhausted,
            "this station has too many waterfall subscriptions still being closed",
        ));
    }
    let pending = reserve(subscriptions, next_ticket, subscription_id, receiver_id);
    Step::Subscribe(id, req, backend, pending)
}

async fn subscribe(
    ctx: &RequestContext,
    id: u64,
    req: v1::SubscribeWaterfall,
    backend: &dyn Backend,
    pending: Pending,
) -> (v1::Response, Option<Media>) {
    let (subscription_id, receiver_id) = (req.subscription_id, req.receiver_id);
    if !pending.after_previous().await {
        release(
            &mut ctx.tables().subscriptions,
            subscription_id,
            pending.ticket,
        );
        return (still_closing(id, "subscription", subscription_id), None);
    }
    let skip = cancelled(&ctx.tables().subscriptions, subscription_id, pending.ticket);
    if skip {
        // Closed while it waited: the backend is not called.
        release(
            &mut ctx.tables().subscriptions,
            subscription_id,
            pending.ticket,
        );
        let subscribed = v1::WaterfallSubscribed {
            subscription_id,
            receiver_id,
            range: req.range,
            bins: req.bins,
            row_interval_ms: req.row_interval_ms,
            quantization: req.quantization,
            compression: req.compression,
        };
        return (ok(id, Some(Payload::WaterfallSubscribed(subscribed))), None);
    }
    match backend.subscribe_waterfall(req).await {
        Err(e) => {
            release(
                &mut ctx.tables().subscriptions,
                subscription_id,
                pending.ticket,
            );
            (backend_error(id, &e), None)
        }
        Ok((mut subscribed, stream)) => {
            subscribed.subscription_id = subscription_id;
            subscribed.receiver_id = receiver_id;
            let stop = CancellationToken::new();
            let keep = promote(
                &mut ctx.tables().subscriptions,
                subscription_id,
                pending.ticket,
                &stop,
            );
            let response = ok(id, Some(Payload::WaterfallSubscribed(subscribed)));
            if keep {
                let media = Media::Rows {
                    subscription_id,
                    stream,
                    stop,
                };
                (response, Some(media))
            } else {
                // Unsubscribed (or the connection ended) while opening: it
                // never goes live.
                let _ = backend.unsubscribe_waterfall(subscription_id).await;
                release(
                    &mut ctx.tables().subscriptions,
                    subscription_id,
                    pending.ticket,
                );
                (response, None)
            }
        }
    }
}

async fn unsubscribe(
    ctx: &RequestContext,
    id: u64,
    subscription_id: u32,
    backend: Option<Arc<dyn Backend>>,
    pending: Pending,
) -> v1::Response {
    let response = match backend {
        Some(backend) => closed(id, backend.unsubscribe_waterfall(subscription_id).await),
        None => ok(id, None),
    };
    release(
        &mut ctx.tables().subscriptions,
        subscription_id,
        pending.ticket,
    );
    response
}

/// Closes `channels` and `subscriptions` in their receivers' backends (the
/// connection ended, §2.3).
pub async fn close_in_backends(receivers: &Receivers, channels: Owned, subscriptions: Owned) {
    for (receiver_id, id) in channels {
        if let Some(b) = receivers.running_backend(receiver_id) {
            if let Err(e) = b.close_channel(id).await {
                tracing::debug!(channel_id = id, error = %e, "closing channel after disconnect");
            }
        }
    }
    for (receiver_id, id) in subscriptions {
        if let Some(b) = receivers.running_backend(receiver_id) {
            if let Err(e) = b.unsubscribe_waterfall(id).await {
                tracing::debug!(subscription_id = id, error = %e, "closing subscription after disconnect");
            }
        }
    }
}

#[cfg(test)]
mod tests;
