// SPDX-License-Identifier: GPL-3.0-only
//! Answers hub requests (station-protocol §3, §4): every `Request` gets
//! exactly one `Response`.
//!
//! | Command | Answer |
//! |---|---|
//! | unknown command or enum value (newer minor) | `UNSUPPORTED` (§3) |
//! | missing required field, `UNSPECIFIED`, `request_id` 0 | `INVALID_ARGUMENT` |
//! | `GetCapabilities` | backend capabilities ∩ local policy ([`effective_capabilities`]) |
//! | `Ping` | `Pong` with the same nonce |
//! | `AdminOp` | `PERMISSION_DENIED` unless listed in `allowed_admin_ops`; if listed, `OK` and logged only (not executed yet) |
//! | `SetFrontend` | `PERMISSION_DENIED` if `allow_frontend_control = false`, else the backend |
//! | `OpenChannel` / `SubscribeWaterfall` | `ALREADY_EXISTS` / `RESOURCE_EXHAUSTED` (local caps) checked here, then the backend |
//! | `UpdateChannel` / `CloseChannel` / `UnsubscribeWaterfall` | `NOT_FOUND` unless open on this connection, then the backend |
//! | `StartMeasurements` (1.1) | `PERMISSION_DENIED` if `[measurements] enabled = false`, `UNSUPPORTED` without measuring, `ALREADY_EXISTS` while a plan runs, else the plan as it will run ([`crate::measure`]) |
//! | `StopMeasurements` (1.1) | `OK`, or `NOT_FOUND` for a plan that isn't running |
//!
//! An accepted `SetFrontend` stops the running plan first
//! (`Event{MEASUREMENT_STOPPED}`): a measurement never straddles a change of
//! settings.
//!
//! Channels and subscriptions belong to one connection ([`Tables`], §2.3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use station_backend::{AudioStream, Backend, BackendError, RowStream};
use station_proto::validate::check_request;
use station_proto::{limits, v1};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::LocalPolicy;
use crate::measure::{self, Measurer, StopReason};

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

/// The channels and subscriptions of one connection.
#[derive(Debug, Default)]
pub struct Tables {
    channels: HashMap<u32, Slot>,
    subscriptions: HashMap<u32, Slot>,
}

impl Tables {
    /// Open channels (not counting ones being opened).
    pub fn open_channels(&self) -> usize {
        self.channels
            .values()
            .filter(|s| matches!(s, Slot::Open { .. }))
            .count()
    }

    /// Open subscriptions (not counting ones being opened).
    pub fn open_subscriptions(&self) -> usize {
        self.subscriptions
            .values()
            .filter(|s| matches!(s, Slot::Open { .. }))
            .count()
    }

    /// Forgets a channel the backend closed by itself, stopping its pump.
    pub fn channel_closed(&mut self, id: u32) {
        if let Some(Slot::Open { stop }) = self.channels.remove(&id) {
            stop.cancel();
        }
    }

    /// Forgets a subscription the backend closed by itself.
    pub fn subscription_closed(&mut self, id: u32) {
        if let Some(Slot::Open { stop }) = self.subscriptions.remove(&id) {
            stop.cancel();
        }
    }

    /// Empties both tables, stopping every pump. Returns the channel and
    /// subscription IDs that were known (open or opening), for closing them
    /// in the backend.
    pub fn take_all(&mut self) -> (Vec<u32>, Vec<u32>) {
        let stop_all = |m: &mut HashMap<u32, Slot>| {
            let mut ids: Vec<u32> = m
                .drain()
                .map(|(id, slot)| {
                    if let Slot::Open { stop } = slot {
                        stop.cancel();
                    }
                    id
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
    /// The DSP backend.
    pub backend: Arc<dyn Backend>,
    /// The owner's policy.
    pub policy: Arc<LocalPolicy>,
    /// The negotiated protocol minor (§3).
    pub minor: u32,
    /// The connection's channel and subscription tables.
    pub tables: Arc<Mutex<Tables>>,
    /// The connection's measurement plan (1.1); `None`: the station doesn't
    /// measure on this connection.
    pub measurer: Option<Arc<Measurer>>,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("policy", &self.policy)
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

/// Capabilities answered to `GetCapabilities`: the backend's, limited by the
/// owner's local caps (§4, §7) and sanitised so the hub's §5 checks pass.
pub fn effective_capabilities(
    backend: &v1::Capabilities,
    policy: &LocalPolicy,
) -> v1::Capabilities {
    let mut caps = backend.clone();
    caps.max_channels = caps.max_channels.min(policy.max_channels);
    if let Some(w) = caps.waterfall.as_mut() {
        w.max_subscriptions = w.max_subscriptions.min(policy.max_waterfall_subscriptions);
    }
    caps.allowed_admin_ops = policy.allowed_admin_ops.iter().map(|k| *k as i32).collect();
    caps.frontend_control_allowed = policy.allow_frontend_control;
    // The station's own claim, never the backend's (see `get_capabilities`).
    caps.measurements = None;
    caps.sdr_model = truncate_utf8(&caps.sdr_model, limits::MAX_SDR_MODEL_BYTES);
    if let Some(f) = caps.frontend.as_mut() {
        f.antenna_ports.truncate(limits::MAX_ANTENNA_PORTS);
        for port in &mut f.antenna_ports {
            *port = truncate_utf8(port, limits::MAX_ANTENNA_PORT_BYTES);
        }
    }
    caps
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
        Command::GetCapabilities(_) => (get_capabilities(ctx, id).await, None),
        Command::Ping(p) => {
            let pong = v1::Pong {
                nonce: p.nonce,
                station_time_unix_ms: wall_clock_ms(),
            };
            (ok(id, Some(Payload::Pong(pong))), None)
        }
        Command::AdminOp(op) => (admin_op(ctx, id, op.kind), None),
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
    match ctx.backend.capabilities().await {
        Ok(caps) => {
            let mut caps = effective_capabilities(&caps, &ctx.policy);
            // 1.1: what the station's measuring accepts.
            if ctx.minor >= limits::MEASUREMENTS_MINOR {
                caps.measurements = measure::limits_for(
                    ctx.measurer.is_some() && ctx.backend.can_measure(),
                    ctx.policy.measurements,
                );
            }
            ok(id, Some(Payload::Capabilities(caps)))
        }
        Err(e) => backend_error(id, &e),
    }
}

fn admin_op(ctx: &RequestContext, id: u64, kind: i32) -> v1::Response {
    let Ok(kind) = v1::AdminOpKind::try_from(kind) else {
        return error(id, v1::ResultCode::Unsupported, "unknown admin operation");
    };
    if ctx.policy.allows_admin_op(kind) {
        // Accepted but not executed; the privileged helper is not implemented yet.
        info!(
            admin_op = kind.as_str_name(),
            "admin operation accepted (allowed_admin_ops); execution is not implemented yet, nothing was done"
        );
        ok(id, None)
    } else {
        warn!(
            admin_op = kind.as_str_name(),
            "admin operation refused: not in allowed_admin_ops"
        );
        error(
            id,
            v1::ResultCode::PermissionDenied,
            "not in allowed_admin_ops of stationd.toml",
        )
    }
}

async fn set_frontend(ctx: &RequestContext, id: u64, req: v1::SetFrontend) -> v1::Response {
    if !ctx.policy.allow_frontend_control {
        return error(
            id,
            v1::ResultCode::PermissionDenied,
            "frontend control is disabled in stationd.toml",
        );
    }
    match ctx.backend.set_frontend(req).await {
        Ok(state) => {
            if let Some(m) = &ctx.measurer {
                m.stop_for(StopReason::FrontendChanged);
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
    let channel_id = req.channel_id;
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
        if t.channels.len() >= usize::try_from(ctx.policy.max_channels).unwrap_or(usize::MAX) {
            return (
                error(
                    id,
                    v1::ResultCode::ResourceExhausted,
                    &format!(
                        "local limit max_channels = {} reached",
                        ctx.policy.max_channels
                    ),
                ),
                None,
            );
        }
        t.channels.insert(
            channel_id,
            Slot::Opening {
                close_requested: false,
            },
        );
    }
    match ctx.backend.open_channel(req).await {
        Err(e) => {
            ctx.tables().channels.remove(&channel_id);
            (backend_error(id, &e), None)
        }
        Ok((mut opened, stream)) => {
            opened.channel_id = channel_id;
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
                let _ = ctx.backend.close_channel(channel_id).await;
                (response, None)
            }
        }
    }
}

/// After a successful open: `Opening` becomes `Open`, unless the hub closed
/// it meanwhile (or the connection ended); then the slot is removed and
/// `false` returned.
fn promote(map: &mut HashMap<u32, Slot>, id: u32, stop: &CancellationToken) -> bool {
    if let Some(
        slot @ Slot::Opening {
            close_requested: false,
        },
    ) = map.get_mut(&id)
    {
        *slot = Slot::Open { stop: stop.clone() };
        true
    } else {
        map.remove(&id);
        false
    }
}

fn is_open(slot: Option<&Slot>) -> bool {
    matches!(slot, Some(Slot::Open { .. }))
}

async fn update_channel(ctx: &RequestContext, id: u64, req: v1::UpdateChannel) -> v1::Response {
    let channel_id = req.channel_id;
    if !is_open(ctx.tables().channels.get(&channel_id)) {
        return error(
            id,
            v1::ResultCode::NotFound,
            &format!("channel {channel_id} is not open"),
        );
    }
    match ctx.backend.update_channel(req).await {
        Ok(mut updated) => {
            updated.channel_id = channel_id;
            ok(id, Some(Payload::ChannelUpdated(updated)))
        }
        Err(e) => backend_error(id, &e),
    }
}

async fn close_channel(ctx: &RequestContext, id: u64, channel_id: u32) -> v1::Response {
    {
        let mut t = ctx.tables();
        match t.channels.get_mut(&channel_id) {
            None => {
                return error(
                    id,
                    v1::ResultCode::NotFound,
                    &format!("channel {channel_id} is not open"),
                );
            }
            Some(Slot::Opening { close_requested }) => {
                *close_requested = true;
                return ok(id, None);
            }
            Some(Slot::Open { stop }) => {
                stop.cancel();
                t.channels.remove(&channel_id);
            }
        }
    }
    match ctx.backend.close_channel(channel_id).await {
        // Already gone in the backend: closed either way.
        Ok(()) => ok(id, None),
        Err(e) if e.code == v1::ResultCode::NotFound => ok(id, None),
        Err(e) => backend_error(id, &e),
    }
}

async fn subscribe(
    ctx: &RequestContext,
    id: u64,
    req: v1::SubscribeWaterfall,
) -> (v1::Response, Option<Media>) {
    let subscription_id = req.subscription_id;
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
        let max = ctx.policy.max_waterfall_subscriptions;
        if t.subscriptions.len() >= usize::try_from(max).unwrap_or(usize::MAX) {
            return (
                error(
                    id,
                    v1::ResultCode::ResourceExhausted,
                    &format!("local limit max_waterfall_subscriptions = {max} reached"),
                ),
                None,
            );
        }
        t.subscriptions.insert(
            subscription_id,
            Slot::Opening {
                close_requested: false,
            },
        );
    }
    match ctx.backend.subscribe_waterfall(req).await {
        Err(e) => {
            ctx.tables().subscriptions.remove(&subscription_id);
            (backend_error(id, &e), None)
        }
        Ok((mut subscribed, stream)) => {
            subscribed.subscription_id = subscription_id;
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
                let _ = ctx.backend.unsubscribe_waterfall(subscription_id).await;
                (response, None)
            }
        }
    }
}

async fn unsubscribe(ctx: &RequestContext, id: u64, subscription_id: u32) -> v1::Response {
    {
        let mut t = ctx.tables();
        match t.subscriptions.get_mut(&subscription_id) {
            None => {
                return error(
                    id,
                    v1::ResultCode::NotFound,
                    &format!("subscription {subscription_id} is not open"),
                );
            }
            Some(Slot::Opening { close_requested }) => {
                *close_requested = true;
                return ok(id, None);
            }
            Some(Slot::Open { stop }) => {
                stop.cancel();
                t.subscriptions.remove(&subscription_id);
            }
        }
    }
    match ctx.backend.unsubscribe_waterfall(subscription_id).await {
        Ok(()) => ok(id, None),
        Err(e) if e.code == v1::ResultCode::NotFound => ok(id, None),
        Err(e) => backend_error(id, &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use station_backend::{ChannelOutput, FakeBackend, FakeConfig};
    use station_proto::validate::sanitize_capabilities;

    fn ctx_with(policy: LocalPolicy, backend: FakeBackend) -> RequestContext {
        RequestContext {
            backend: Arc::new(backend),
            policy: Arc::new(policy),
            minor: 0,
            tables: Arc::default(),
            measurer: None,
        }
    }

    fn ctx() -> RequestContext {
        ctx_with(LocalPolicy::default(), FakeBackend::default())
    }

    fn req(id: u64, command: Command) -> v1::Request {
        v1::Request {
            request_id: id,
            command: Some(command),
        }
    }

    fn code(r: &v1::Response) -> v1::ResultCode {
        r.result
            .as_ref()
            .and_then(|r| v1::ResultCode::try_from(r.code).ok())
            .unwrap_or(v1::ResultCode::Unspecified)
    }

    fn open(channel_id: u32) -> Command {
        Command::OpenChannel(v1::OpenChannel {
            channel_id,
            frequency_hz: 7_100_000,
            mode: v1::Mode::Usb as i32,
            low_cut_hz: 200,
            high_cut_hz: 2800,
            audio: Some(v1::AudioParams {
                codec: v1::AudioCodec::Opus as i32,
                bitrate_bps: 16_000,
                sample_rate_hz: 12_000,
                frame_duration_ms: 20,
            }),
            squelch_snr_db: None,
        })
    }

    fn subscribe_cmd(subscription_id: u32) -> Command {
        Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            subscription_id,
            range: Some(v1::FrequencyRange {
                start_hz: 7_000_000,
                end_hz: 7_200_000,
            }),
            bins: 128,
            row_interval_ms: 100,
            quantization: None,
            compression: v1::WaterfallCompression::Zstd as i32,
        })
    }

    #[test]
    fn capabilities_are_intersected_with_local_policy() {
        let backend = FakeConfig::default().capabilities;
        let policy = LocalPolicy {
            allow_frontend_control: false,
            allowed_admin_ops: vec![v1::AdminOpKind::Update],
            max_channels: 4,
            max_waterfall_subscriptions: 1,
            measurements: true,
        };
        let caps = effective_capabilities(&backend, &policy);
        assert_eq!(caps.max_channels, 4);
        assert_eq!(caps.waterfall.map(|w| w.max_subscriptions), Some(1));
        assert_eq!(caps.allowed_admin_ops, vec![v1::AdminOpKind::Update as i32]);
        assert!(!caps.frontend_control_allowed);
        assert_eq!(caps.tunable_range, backend.tunable_range);
        // The local cap never raises the backend's.
        let generous = LocalPolicy {
            max_channels: 64,
            max_waterfall_subscriptions: 16,
            ..LocalPolicy::default()
        };
        let caps = effective_capabilities(&backend, &generous);
        assert_eq!(caps.max_channels, backend.max_channels);
        assert_eq!(caps.waterfall.map(|w| w.max_subscriptions), Some(4));
        assert!(caps.frontend_control_allowed);
        assert!(sanitize_capabilities(&caps, 0).is_ok());
    }

    #[test]
    fn oversized_backend_strings_are_trimmed() {
        let mut backend = FakeConfig::default().capabilities;
        backend.sdr_model = "é".repeat(100);
        backend.frontend = Some(v1::FrontendControls {
            antenna_ports: vec!["x".repeat(40); 12],
            ..Default::default()
        });
        let caps = effective_capabilities(&backend, &LocalPolicy::default());
        assert!(caps.sdr_model.len() <= limits::MAX_SDR_MODEL_BYTES);
        let ports = caps.frontend.expect("frontend").antenna_ports;
        assert_eq!(ports.len(), limits::MAX_ANTENNA_PORTS);
        assert!(
            ports
                .iter()
                .all(|p| p.len() == limits::MAX_ANTENNA_PORT_BYTES)
        );
        let mut caps = FakeConfig::default().capabilities;
        caps.waterfall = None;
        assert!(
            effective_capabilities(&caps, &LocalPolicy::default())
                .waterfall
                .is_none()
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("abc", 5), "abc");
        assert_eq!(truncate_utf8("abcdef", 3), "abc");
        assert_eq!(truncate_utf8("aé", 2), "a");
    }

    #[tokio::test]
    async fn dispatch_table() {
        let c = ctx();
        let (r, _) = dispatch(&c, req(1, Command::GetCapabilities(v1::GetCapabilities {}))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(matches!(r.payload, Some(Payload::Capabilities(_))));
        let (r, _) = dispatch(
            &c,
            req(
                2,
                Command::Ping(v1::Ping {
                    nonce: 77,
                    hub_time_unix_ms: 0,
                }),
            ),
        )
        .await;
        assert_eq!(r.request_id, 2);
        let Some(Payload::Pong(pong)) = r.payload else {
            panic!("pong expected")
        };
        assert_eq!(pong.nonce, 77);
        assert!(pong.station_time_unix_ms > 0);
        // §3: unknown command, unknown enum value, missing field, id 0.
        let (r, _) = dispatch(
            &c,
            v1::Request {
                request_id: 3,
                command: None,
            },
        )
        .await;
        assert_eq!(code(&r), v1::ResultCode::Unsupported);
        let (r, _) = dispatch(&c, req(4, Command::AdminOp(v1::AdminOp { kind: 99 }))).await;
        assert_eq!(code(&r), v1::ResultCode::Unsupported);
        let (r, _) = dispatch(&c, req(5, Command::AdminOp(v1::AdminOp { kind: 0 }))).await;
        assert_eq!(code(&r), v1::ResultCode::InvalidArgument);
        let (r, _) = dispatch(&c, req(0, Command::Ping(v1::Ping::default()))).await;
        assert_eq!(code(&r), v1::ResultCode::InvalidArgument);
        // Unknown ids.
        for (id, cmd) in [
            (6, Command::CloseChannel(v1::CloseChannel { channel_id: 9 })),
            (
                7,
                Command::UpdateChannel(v1::UpdateChannel {
                    channel_id: 9,
                    ..Default::default()
                }),
            ),
            (
                8,
                Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 9 }),
            ),
        ] {
            let (r, m) = dispatch(&c, req(id, cmd)).await;
            assert_eq!(code(&r), v1::ResultCode::NotFound);
            assert_eq!(r.request_id, id);
            assert!(m.is_none());
        }
    }

    #[tokio::test]
    async fn admin_ops_follow_the_allow_list() {
        let c = ctx();
        for kind in [
            v1::AdminOpKind::RestartStationd,
            v1::AdminOpKind::RestartBackend,
            v1::AdminOpKind::Update,
            v1::AdminOpKind::RebootHost,
        ] {
            let (r, _) = dispatch(
                &c,
                req(1, Command::AdminOp(v1::AdminOp { kind: kind as i32 })),
            )
            .await;
            assert_eq!(code(&r), v1::ResultCode::PermissionDenied, "{kind:?}");
        }
        let c = ctx_with(
            LocalPolicy {
                allowed_admin_ops: vec![v1::AdminOpKind::RestartBackend],
                ..LocalPolicy::default()
            },
            FakeBackend::default(),
        );
        let allowed = Command::AdminOp(v1::AdminOp {
            kind: v1::AdminOpKind::RestartBackend as i32,
        });
        let (r, _) = dispatch(&c, req(1, allowed)).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(r.payload.is_none());
        assert_eq!(
            admin_op(&c, 2, 1234).result.map(|r| r.code),
            Some(v1::ResultCode::Unsupported as i32)
        );
    }

    #[tokio::test]
    async fn set_frontend_needs_permission() {
        let deny = ctx_with(
            LocalPolicy {
                allow_frontend_control: false,
                ..LocalPolicy::default()
            },
            FakeBackend::default(),
        );
        let cmd = || {
            Command::SetFrontend(v1::SetFrontend {
                gain_db: Some(10.0),
                ..Default::default()
            })
        };
        let (r, _) = dispatch(&deny, req(1, cmd())).await;
        assert_eq!(code(&r), v1::ResultCode::PermissionDenied);
        let (r, _) = dispatch(&ctx(), req(1, cmd())).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(matches!(r.payload, Some(Payload::FrontendState(_))));
        let bad = Command::SetFrontend(v1::SetFrontend {
            gain_db: Some(1000.0),
            ..Default::default()
        });
        let (r, _) = dispatch(&ctx(), req(2, bad)).await;
        assert_eq!(code(&r), v1::ResultCode::OutOfRange);
    }

    #[tokio::test]
    async fn channel_lifecycle_and_local_caps() {
        let backend = FakeBackend::default();
        let c = ctx_with(
            LocalPolicy {
                max_channels: 1,
                ..LocalPolicy::default()
            },
            backend.clone(),
        );
        let (r, media) = dispatch(&c, req(1, open(5))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        let Some(Media::Audio {
            channel_id,
            mut stream,
            stop,
        }) = media
        else {
            panic!("audio expected")
        };
        assert_eq!(channel_id, 5);
        assert!(matches!(stream.next().await, Some(ChannelOutput::Audio(_))));
        assert_eq!(c.tables().open_channels(), 1);
        let (r, _) = dispatch(&c, req(2, open(5))).await;
        assert_eq!(code(&r), v1::ResultCode::AlreadyExists);
        let (r, _) = dispatch(&c, req(3, open(6))).await;
        assert_eq!(code(&r), v1::ResultCode::ResourceExhausted);
        let upd = Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 5,
            frequency_hz: Some(7_050_000),
            ..Default::default()
        });
        let (r, _) = dispatch(&c, req(4, upd)).await;
        assert!(
            matches!(r.payload, Some(Payload::ChannelUpdated(ref u)) if u.frequency_hz == 7_050_000)
        );
        let bad_upd = Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 5,
            frequency_hz: Some(1),
            ..Default::default()
        });
        let (r, _) = dispatch(&c, req(5, bad_upd)).await;
        assert_eq!(code(&r), v1::ResultCode::OutOfRange);
        let (r, _) = dispatch(
            &c,
            req(6, Command::CloseChannel(v1::CloseChannel { channel_id: 5 })),
        )
        .await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(stop.is_cancelled());
        assert!(backend.channel_ids().is_empty());
        assert_eq!(c.tables().open_channels(), 0);
        // Backend refusal frees the slot.
        let mut bad = open(7);
        if let Command::OpenChannel(o) = &mut bad {
            o.frequency_hz = 1;
        }
        let (r, m) = dispatch(&c, req(7, bad)).await;
        assert_eq!(code(&r), v1::ResultCode::OutOfRange);
        assert!(m.is_none());
        assert!(c.tables().channels.is_empty());
    }

    /// A backend whose `open_channel`/`subscribe_waterfall` wait for a gate.
    struct Gated {
        inner: FakeBackend,
        gate: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl Backend for Gated {
        fn info(&self) -> v1::BackendInfo {
            self.inner.info()
        }
        async fn capabilities(&self) -> Result<v1::Capabilities, BackendError> {
            self.inner.capabilities().await
        }
        async fn open_channel(
            &self,
            req: v1::OpenChannel,
        ) -> Result<(v1::ChannelOpened, AudioStream), BackendError> {
            let _permit = self.gate.acquire().await;
            self.inner.open_channel(req).await
        }
        async fn update_channel(
            &self,
            req: v1::UpdateChannel,
        ) -> Result<v1::ChannelUpdated, BackendError> {
            self.inner.update_channel(req).await
        }
        async fn close_channel(&self, id: u32) -> Result<(), BackendError> {
            self.inner.close_channel(id).await
        }
        async fn set_frontend(
            &self,
            req: v1::SetFrontend,
        ) -> Result<v1::FrontendState, BackendError> {
            self.inner.set_frontend(req).await
        }
        async fn subscribe_waterfall(
            &self,
            req: v1::SubscribeWaterfall,
        ) -> Result<(v1::WaterfallSubscribed, RowStream), BackendError> {
            let _permit = self.gate.acquire().await;
            self.inner.subscribe_waterfall(req).await
        }
        async fn unsubscribe_waterfall(&self, id: u32) -> Result<(), BackendError> {
            self.inner.unsubscribe_waterfall(id).await
        }
        fn health(&self) -> station_backend::Health {
            self.inner.health()
        }
        fn events(&self) -> station_backend::EventStream {
            self.inner.events()
        }
    }

    #[tokio::test]
    async fn close_while_opening_closes_after_open() {
        let inner = FakeBackend::default();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let c = RequestContext {
            backend: Arc::new(Gated {
                inner: inner.clone(),
                gate: Arc::clone(&gate),
            }),
            policy: Arc::new(LocalPolicy::default()),
            minor: 0,
            tables: Arc::default(),
            measurer: None,
        };
        assert_eq!(c.backend.info(), inner.info());
        let opening = tokio::spawn({
            let c = c.clone();
            async move { dispatch(&c, req(1, open(9))).await }
        });
        let subscribing = tokio::spawn({
            let c = c.clone();
            async move { dispatch(&c, req(2, subscribe_cmd(4))).await }
        });
        while c.tables().channels.is_empty() || c.tables().subscriptions.is_empty() {
            tokio::task::yield_now().await;
        }
        // Updating a channel that is still opening: not found (yet).
        let upd = Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 9,
            ..Default::default()
        });
        let (r, _) = dispatch(&c, req(3, upd)).await;
        assert_eq!(code(&r), v1::ResultCode::NotFound);
        let (r, _) = dispatch(
            &c,
            req(4, Command::CloseChannel(v1::CloseChannel { channel_id: 9 })),
        )
        .await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        let un = Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 4 });
        let (r, _) = dispatch(&c, req(5, un)).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        gate.add_permits(2);
        let (r, media) = opening.await.expect("join");
        assert_eq!(code(&r), v1::ResultCode::Ok, "the open itself succeeded");
        assert!(media.is_none(), "but no audio is pumped");
        let (r, media) = subscribing.await.expect("join");
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(media.is_none());
        assert!(inner.channel_ids().is_empty(), "closed in the backend");
        assert!(inner.subscription_ids().is_empty());
        assert!(c.tables().channels.is_empty() && c.tables().subscriptions.is_empty());
        // Capabilities and the rest go straight through.
        let (r, _) = dispatch(&c, req(6, Command::GetCapabilities(v1::GetCapabilities {}))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(c.backend.health().frontend.is_some());
        let _events = c.backend.events();
        let (r, _) = dispatch(&c, req(7, Command::SetFrontend(v1::SetFrontend::default()))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
    }

    #[tokio::test]
    async fn subscription_lifecycle() {
        let backend = FakeBackend::default();
        let c = ctx_with(
            LocalPolicy {
                max_waterfall_subscriptions: 1,
                ..LocalPolicy::default()
            },
            backend.clone(),
        );
        let (r, media) = dispatch(&c, req(1, subscribe_cmd(3))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(matches!(
            media,
            Some(Media::Rows {
                subscription_id: 3,
                ..
            })
        ));
        assert_eq!(
            format!("{:?}", media.expect("media")),
            "Rows { subscription_id: 3, .. }"
        );
        assert_eq!(c.tables().open_subscriptions(), 1);
        let (r, _) = dispatch(&c, req(2, subscribe_cmd(3))).await;
        assert_eq!(code(&r), v1::ResultCode::AlreadyExists);
        let (r, _) = dispatch(&c, req(3, subscribe_cmd(4))).await;
        assert_eq!(code(&r), v1::ResultCode::ResourceExhausted);
        let un = |id| {
            Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall {
                subscription_id: id,
            })
        };
        let (r, _) = dispatch(&c, req(4, un(3))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(backend.subscription_ids().is_empty());
        // Backend refusal.
        let mut bad = subscribe_cmd(5);
        if let Command::SubscribeWaterfall(s) = &mut bad {
            s.bins = 1;
        }
        let (r, _) = dispatch(&c, req(5, bad)).await;
        assert_eq!(code(&r), v1::ResultCode::OutOfRange);
        assert!(c.tables().subscriptions.is_empty());
        // Opening slot + unsubscribe.
        c.tables().subscriptions.insert(
            6,
            Slot::Opening {
                close_requested: false,
            },
        );
        let (r, _) = dispatch(&c, req(6, un(6))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(matches!(
            c.tables().subscriptions.get(&6),
            Some(Slot::Opening {
                close_requested: true
            })
        ));
    }

    #[tokio::test]
    async fn tables_teardown() {
        let backend = FakeBackend::default();
        let c = ctx_with(LocalPolicy::default(), backend.clone());
        let (_, m1) = dispatch(&c, req(1, open(1))).await;
        let (_, m2) = dispatch(&c, req(2, subscribe_cmd(2))).await;
        c.tables().channels.insert(
            3,
            Slot::Opening {
                close_requested: false,
            },
        );
        let (channels, subs) = c.tables().take_all();
        assert_eq!(channels, vec![1, 3]);
        assert_eq!(subs, vec![2]);
        for m in [m1, m2].into_iter().flatten() {
            match m {
                Media::Audio { stop, .. } | Media::Rows { stop, .. } => {
                    assert!(stop.is_cancelled());
                }
            }
        }
        let (_, m) = dispatch(&c, req(3, open(4))).await;
        c.tables().channel_closed(4);
        assert!(matches!(m, Some(Media::Audio { ref stop, .. }) if stop.is_cancelled()));
        let (_, m) = dispatch(&c, req(4, subscribe_cmd(5))).await;
        c.tables().subscription_closed(5);
        assert!(matches!(m, Some(Media::Rows { ref stop, .. }) if stop.is_cancelled()));
        assert!(format!("{c:?}").contains("RequestContext"));
    }

    #[test]
    fn backend_errors_never_say_ok() {
        let e = BackendError::new(v1::ResultCode::Ok, "weird");
        assert_eq!(code(&backend_error(1, &e)), v1::ResultCode::BackendError);
        let long = BackendError::backend("x".repeat(1000));
        let r = backend_error(1, &long);
        assert_eq!(
            r.result.map(|r| r.message.len()),
            Some(limits::MAX_DETAIL_BYTES)
        );
    }
}
