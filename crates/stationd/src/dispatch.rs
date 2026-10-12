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
//! | `UpdateChannel` / `CloseChannel` / `UnsubscribeWaterfall` | `NOT_FOUND` unless open on this connection, then the backend of its receiver |
//! | `StartMeasurements` | `UNSUPPORTED` without measuring, `ALREADY_EXISTS` while a plan runs, else the plan as it will run ([`crate::measure`]) |
//! | `StopMeasurements` | `OK`, or `NOT_FOUND` for a plan that isn't running |
//!
//! An accepted `SetFrontend` stops the running plan first
//! (`Event{MEASUREMENT_STOPPED}`): a measurement never straddles a change of
//! settings.
//!
//! Channels and subscriptions belong to one connection ([`Tables`], §2.3),
//! each on the receiver it was opened on (§4).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use station_backend::{AudioStream, BackendError, RowStream};
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
    /// before the open finished.
    Opening { close_requested: bool },
    /// Open; cancelling `stop` ends the media pump.
    Open { stop: CancellationToken },
}

/// A channel or subscription, on its receiver.
#[derive(Debug)]
struct Entry {
    receiver_id: u32,
    slot: Slot,
}

impl Entry {
    fn opening(receiver_id: u32) -> Self {
        Self {
            receiver_id,
            slot: Slot::Opening {
                close_requested: false,
            },
        }
    }

    fn is_open(&self) -> bool {
        matches!(self.slot, Slot::Open { .. })
    }

    fn stop(self) {
        if let Slot::Open { stop } = self.slot {
            stop.cancel();
        }
    }
}

/// The channels and subscriptions of one connection.
#[derive(Debug, Default)]
pub struct Tables {
    channels: HashMap<u32, Entry>,
    subscriptions: HashMap<u32, Entry>,
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

    /// Forgets a channel the backend closed by itself, stopping its pump.
    pub fn channel_closed(&mut self, id: u32) {
        if let Some(e) = self.channels.remove(&id) {
            e.stop();
        }
    }

    /// Forgets a subscription the backend closed by itself.
    pub fn subscription_closed(&mut self, id: u32) {
        if let Some(e) = self.subscriptions.remove(&id) {
            e.stop();
        }
    }

    /// Forgets everything on `receiver_id`, whose backend is gone
    /// (`BACKEND_RESTARTED`), stopping the pumps.
    pub fn receiver_gone(&mut self, receiver_id: u32) {
        for map in [&mut self.channels, &mut self.subscriptions] {
            let gone: Vec<u32> = map
                .iter()
                .filter(|(_, e)| e.receiver_id == receiver_id)
                .map(|(id, _)| *id)
                .collect();
            for id in gone {
                if let Some(e) = map.remove(&id) {
                    e.stop();
                }
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

/// Answers `req`. Always returns exactly one response for `req.request_id`,
/// plus media to pump when a channel or subscription was opened.
pub async fn dispatch(ctx: &RequestContext, req: v1::Request) -> (v1::Response, Option<Media>) {
    let (response, media) = answer(ctx, req).await;
    tracing::debug!(
        request_id = response.request_id,
        code = response.result.as_ref().map_or(0, |r| r.code),
        "answered request"
    );
    (response, media)
}

async fn answer(ctx: &RequestContext, req: v1::Request) -> (v1::Response, Option<Media>) {
    let id = req.request_id;
    if let Err(rejection) = check_request(&req, ctx.minor) {
        let response = v1::Response {
            request_id: id,
            result: Some(rejection.to_result()),
            payload: None,
        };
        return (response, None);
    }
    let Some(command) = req.command else {
        // check_request rejects a missing command; unreachable in practice.
        return (error(id, v1::ResultCode::Unsupported, "no command"), None);
    };
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
        Command::OpenChannel(o) => open_channel(ctx, id, o).await,
        Command::UpdateChannel(u) => (update_channel(ctx, id, u).await, None),
        Command::CloseChannel(c) => (close_channel(ctx, id, c.channel_id).await, None),
        Command::SubscribeWaterfall(s) => subscribe(ctx, id, s).await,
        Command::UnsubscribeWaterfall(u) => (unsubscribe(ctx, id, u.subscription_id).await, None),
        Command::StartMeasurements(m) => (start_measurements(ctx, id, m).await, None),
        Command::StopMeasurements(m) => (stop_measurements(ctx, id, m.measurement_id), None),
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

async fn open_channel(
    ctx: &RequestContext,
    id: u64,
    req: v1::OpenChannel,
) -> (v1::Response, Option<Media>) {
    let (channel_id, receiver_id) = (req.channel_id, req.receiver_id);
    let Some(backend) = ctx.receivers.running_backend(receiver_id) else {
        return (not_running(id, receiver_id), None);
    };
    {
        let mut t = ctx.tables();
        if t.channels.contains_key(&channel_id) {
            return (
                error(
                    id,
                    v1::ResultCode::AlreadyExists,
                    &format!("channel {channel_id} is already open"),
                ),
                None,
            );
        }
        let on_receiver = t
            .channels
            .values()
            .filter(|e| e.receiver_id == receiver_id)
            .count();
        if on_receiver >= usize::try_from(limits::CHANNELS_PER_RECEIVER).unwrap_or(usize::MAX) {
            return (
                error(
                    id,
                    v1::ResultCode::ResourceExhausted,
                    &format!(
                        "receiver {receiver_id} has all its {} channels in use",
                        limits::CHANNELS_PER_RECEIVER
                    ),
                ),
                None,
            );
        }
        t.channels.insert(channel_id, Entry::opening(receiver_id));
    }
    match backend.open_channel(req).await {
        Err(e) => {
            ctx.tables().channels.remove(&channel_id);
            (backend_error(id, &e), None)
        }
        Ok((mut opened, stream)) => {
            opened.channel_id = channel_id;
            opened.receiver_id = receiver_id;
            let stop = CancellationToken::new();
            let keep = promote(&mut ctx.tables().channels, channel_id, &stop);
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
                (response, None)
            }
        }
    }
}

/// After a successful open: `Opening` becomes `Open`, unless the hub closed
/// it meanwhile (or the connection ended, or its receiver went); then the
/// entry is removed and `false` returned.
fn promote(map: &mut HashMap<u32, Entry>, id: u32, stop: &CancellationToken) -> bool {
    if let Some(
        entry @ Entry {
            slot: Slot::Opening {
                close_requested: false,
            },
            ..
        },
    ) = map.get_mut(&id)
    {
        entry.slot = Slot::Open { stop: stop.clone() };
        true
    } else {
        map.remove(&id);
        false
    }
}

/// The receiver of an open channel or subscription.
fn open_on(map: &HashMap<u32, Entry>, id: u32) -> Option<u32> {
    map.get(&id).filter(|e| e.is_open()).map(|e| e.receiver_id)
}

async fn update_channel(ctx: &RequestContext, id: u64, req: v1::UpdateChannel) -> v1::Response {
    let channel_id = req.channel_id;
    let backend = open_on(&ctx.tables().channels, channel_id)
        .and_then(|rx| ctx.receivers.running_backend(rx));
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

/// What closing an ID on behalf of the hub leaves to do.
enum Closing {
    /// Close it in this receiver's backend.
    InBackend(u32),
    /// Nothing more: this is the answer.
    Answered(v1::Response),
}

/// Closes `id` in `map` on behalf of the hub.
fn close_entry(map: &mut HashMap<u32, Entry>, id: u32, request_id: u64, what: &str) -> Closing {
    match map.get_mut(&id) {
        None => Closing::Answered(error(
            request_id,
            v1::ResultCode::NotFound,
            &format!("{what} {id} is not open"),
        )),
        Some(Entry {
            slot: Slot::Opening { close_requested },
            ..
        }) => {
            *close_requested = true;
            Closing::Answered(ok(request_id, None))
        }
        Some(Entry {
            slot: Slot::Open { .. },
            ..
        }) => Closing::InBackend(map.remove(&id).map_or(0, |e| {
            let receiver_id = e.receiver_id;
            e.stop();
            receiver_id
        })),
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

async fn close_channel(ctx: &RequestContext, id: u64, channel_id: u32) -> v1::Response {
    let receiver_id = match close_entry(&mut ctx.tables().channels, channel_id, id, "channel") {
        Closing::InBackend(r) => r,
        Closing::Answered(response) => return response,
    };
    match ctx.receivers.running_backend(receiver_id) {
        Some(backend) => closed(id, backend.close_channel(channel_id).await),
        // Its receiver went: the channel went with it.
        None => ok(id, None),
    }
}

async fn subscribe(
    ctx: &RequestContext,
    id: u64,
    req: v1::SubscribeWaterfall,
) -> (v1::Response, Option<Media>) {
    let (subscription_id, receiver_id) = (req.subscription_id, req.receiver_id);
    let Some(backend) = ctx.receivers.running_backend(receiver_id) else {
        return (not_running(id, receiver_id), None);
    };
    let max = limits::waterfall_subscriptions_claim(ctx.receivers.running().len());
    {
        let mut t = ctx.tables();
        if t.subscriptions.contains_key(&subscription_id) {
            return (
                error(
                    id,
                    v1::ResultCode::AlreadyExists,
                    &format!("subscription {subscription_id} is already open"),
                ),
                None,
            );
        }
        if t.subscriptions.len() >= usize::try_from(max).unwrap_or(usize::MAX) {
            return (
                error(
                    id,
                    v1::ResultCode::ResourceExhausted,
                    &format!("all {max} waterfall subscriptions of this station are in use"),
                ),
                None,
            );
        }
        t.subscriptions
            .insert(subscription_id, Entry::opening(receiver_id));
    }
    match backend.subscribe_waterfall(req).await {
        Err(e) => {
            ctx.tables().subscriptions.remove(&subscription_id);
            (backend_error(id, &e), None)
        }
        Ok((mut subscribed, stream)) => {
            subscribed.subscription_id = subscription_id;
            subscribed.receiver_id = receiver_id;
            let stop = CancellationToken::new();
            let keep = promote(&mut ctx.tables().subscriptions, subscription_id, &stop);
            let response = ok(id, Some(Payload::WaterfallSubscribed(subscribed)));
            if keep {
                let media = Media::Rows {
                    subscription_id,
                    stream,
                    stop,
                };
                (response, Some(media))
            } else {
                let _ = backend.unsubscribe_waterfall(subscription_id).await;
                (response, None)
            }
        }
    }
}

async fn unsubscribe(ctx: &RequestContext, id: u64, subscription_id: u32) -> v1::Response {
    let receiver_id = match close_entry(
        &mut ctx.tables().subscriptions,
        subscription_id,
        id,
        "subscription",
    ) {
        Closing::InBackend(r) => r,
        Closing::Answered(response) => return response,
    };
    match ctx.receivers.running_backend(receiver_id) {
        Some(backend) => closed(id, backend.unsubscribe_waterfall(subscription_id).await),
        None => ok(id, None),
    }
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
