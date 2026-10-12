// SPDX-License-Identifier: MIT
//! Stateless checks of single messages (§5 Size, Strings, Enums, floats,
//! payload sizes). The stateful rules live in [`super::Session`].

use prost::Message as _;

use std::collections::HashSet;

use super::capabilities::sanitize_capabilities;
use super::measure::{check_measurements_started, check_noise_report};
use super::receivers::{check_hardware, check_receivers_configured};
use super::{MessageKind, ViolationKind};
use crate::limits;
use crate::v1;

/// Decodes one `StationMessage` after enforcing the §5 size limit.
///
/// `max_bytes` is normally [`limits::MAX_MESSAGE_BYTES`]; larger values are
/// capped to it. prost rejects invalid UTF-8 in `string` fields, so a
/// successful decode also satisfies the UTF-8 rule of §5 Strings.
pub fn decode_station_message(
    bytes: &[u8],
    max_bytes: usize,
) -> Result<v1::StationMessage, ViolationKind> {
    let max = max_bytes.min(limits::MAX_MESSAGE_BYTES);
    if bytes.len() > max {
        return Err(ViolationKind::MessageTooLarge {
            len: bytes.len(),
            max,
        });
    }
    v1::StationMessage::decode(bytes).map_err(|_| ViolationKind::Decode)
}

/// Checks everything about one message that needs no session state: string
/// caps, enums known in `minor` and not `UNSPECIFIED` where required, finite
/// floats, payload sizes, receiver IDs listed once, the fields each event
/// kind requires, and the sanity of every response payload (including
/// [`sanitize_capabilities`]).
///
/// It does not check the size (see [`decode_station_message`]), ordering,
/// request matching, ID lookups (channels, subscriptions, plans, configured
/// receivers) or rates: [`super::Session::check`] does.
pub fn check_message(msg: &v1::StationMessage, minor: u32) -> Result<(), ViolationKind> {
    use v1::station_message::Body;
    match &msg.body {
        None => Err(ViolationKind::EmptyMessage),
        Some(Body::Hello(h)) => check_hello(h),
        Some(Body::Response(r)) => check_response(r, minor),
        Some(Body::Audio(a)) => check_audio(a),
        Some(Body::Waterfall(w)) => check_waterfall_row(w),
        Some(Body::Signal(s)) => check_signal(s),
        Some(Body::Status(s)) => check_status(s, minor),
        Some(Body::Event(e)) => check_event(e, minor),
        Some(Body::Noise(n)) => check_noise_report(n),
    }
}

/// `Hello` is sent before any version is negotiated, so a station with a
/// newer minor may name a backend kind this side does not know yet. The kind
/// is informational: unknown values are accepted, only `UNSPECIFIED` is
/// rejected.
fn check_hello(h: &v1::Hello) -> Result<(), ViolationKind> {
    if h.protocol_version.is_none() {
        return Err(missing("hello.protocol_version"));
    }
    check_len(
        "hello.daemon_version",
        &h.daemon_version,
        limits::MAX_VERSION_BYTES,
    )?;
    check_len("hello.platform", &h.platform, limits::MAX_PLATFORM_BYTES)?;
    let backend = h.backend.as_ref().ok_or(missing("hello.backend"))?;
    if backend.kind == 0 {
        return Err(ViolationKind::UnspecifiedEnum {
            field: "hello.backend.kind",
        });
    }
    check_len(
        "hello.backend.version",
        &backend.version,
        limits::MAX_VERSION_BYTES,
    )
}

fn check_response(r: &v1::Response, minor: u32) -> Result<(), ViolationKind> {
    use v1::response::Payload;
    let result = r.result.as_ref().ok_or(missing("response.result"))?;
    check_enum::<v1::ResultCode>("response.result.code", result.code, minor, true)?;
    check_len(
        "response.result.message",
        &result.message,
        limits::MAX_DETAIL_BYTES,
    )?;
    match &r.payload {
        None | Some(Payload::Pong(_)) => Ok(()),
        Some(Payload::Capabilities(c)) => sanitize_capabilities(c, minor).map(drop),
        Some(Payload::ChannelOpened(c)) => check_channel_opened(c, minor),
        Some(Payload::ChannelUpdated(c)) => check_channel_updated(c, minor),
        Some(Payload::FrontendState(f)) => check_frontend_state(f),
        Some(Payload::WaterfallSubscribed(w)) => check_waterfall_subscribed(w, minor),
        Some(Payload::MeasurementsStarted(m)) => check_measurements_started(m, minor),
        Some(Payload::Hardware(h)) => check_hardware(h, minor),
        Some(Payload::ReceiversConfigured(r)) => check_receivers_configured(r, minor),
    }
}

fn check_channel_opened(c: &v1::ChannelOpened, minor: u32) -> Result<(), ViolationKind> {
    check_enum::<v1::Mode>("channel_opened.mode", c.mode, minor, true)?;
    check_frequency("channel_opened.frequency_hz", c.frequency_hz)?;
    check_cuts("channel_opened.low_cut_hz", c.low_cut_hz, c.high_cut_hz)?;
    let audio = c.audio.as_ref().ok_or(missing("channel_opened.audio"))?;
    check_audio_params(audio, minor)
}

/// common.proto `AudioParams`: codec known, and bitrate, sample rate and
/// frame duration among the allowed values.
fn check_audio_params(a: &v1::AudioParams, minor: u32) -> Result<(), ViolationKind> {
    check_enum::<v1::AudioCodec>("channel_opened.audio.codec", a.codec, minor, true)?;
    if !limits::AUDIO_BITRATE_BPS.contains(&a.bitrate_bps) {
        return Err(bounds("channel_opened.audio.bitrate_bps"));
    }
    if !limits::AUDIO_SAMPLE_RATES_HZ.contains(&a.sample_rate_hz) {
        return Err(bounds("channel_opened.audio.sample_rate_hz"));
    }
    if !limits::AUDIO_FRAME_DURATIONS_MS.contains(&a.frame_duration_ms) {
        return Err(bounds("channel_opened.audio.frame_duration_ms"));
    }
    Ok(())
}

fn check_channel_updated(c: &v1::ChannelUpdated, minor: u32) -> Result<(), ViolationKind> {
    check_enum::<v1::Mode>("channel_updated.mode", c.mode, minor, true)?;
    check_frequency("channel_updated.frequency_hz", c.frequency_hz)?;
    check_cuts("channel_updated.low_cut_hz", c.low_cut_hz, c.high_cut_hz)
}

pub(crate) fn check_frontend_state(f: &v1::FrontendState) -> Result<(), ViolationKind> {
    check_finite("frontend_state.gain_db", f.gain_db)?;
    check_finite("frontend_state.attenuation_db", f.attenuation_db)?;
    check_len(
        "frontend_state.antenna_port",
        &f.antenna_port,
        limits::MAX_ANTENNA_PORT_BYTES,
    )?;
    check_frequency("frontend_state.center_frequency_hz", f.center_frequency_hz)?;
    if let Some(range) = &f.tunable_range {
        check_range("frontend_state.tunable_range", range)?;
    }
    Ok(())
}

fn check_waterfall_subscribed(
    w: &v1::WaterfallSubscribed,
    minor: u32,
) -> Result<(), ViolationKind> {
    let range = w
        .range
        .as_ref()
        .ok_or(missing("waterfall_subscribed.range"))?;
    check_range("waterfall_subscribed.range", range)?;
    if !(limits::MIN_BINS..=limits::MAX_BINS).contains(&w.bins) {
        return Err(bounds("waterfall_subscribed.bins"));
    }
    if w.row_interval_ms == 0 {
        return Err(bounds("waterfall_subscribed.row_interval_ms"));
    }
    let q = w
        .quantization
        .as_ref()
        .ok_or(missing("waterfall_subscribed.quantization"))?;
    check_finite("waterfall_subscribed.quantization.offset_db", q.offset_db)?;
    check_finite("waterfall_subscribed.quantization.step_db", q.step_db)?;
    check_enum::<v1::WaterfallCompression>(
        "waterfall_subscribed.compression",
        w.compression,
        minor,
        true,
    )
}

fn check_audio(a: &v1::AudioFrame) -> Result<(), ViolationKind> {
    if (limits::MIN_OPUS_PACKET..=limits::MAX_OPUS_PACKET).contains(&a.payload.len()) {
        Ok(())
    } else {
        Err(ViolationKind::AudioPayloadSize {
            len: a.payload.len(),
        })
    }
}

fn check_waterfall_row(w: &v1::WaterfallRow) -> Result<(), ViolationKind> {
    if w.payload.len() > limits::MAX_WATERFALL_COMPRESSED {
        return Err(ViolationKind::WaterfallPayloadTooLarge {
            len: w.payload.len(),
        });
    }
    Ok(())
}

fn check_signal(s: &v1::ChannelSignal) -> Result<(), ViolationKind> {
    check_finite("signal.snr_db", s.snr_db)?;
    check_finite("signal.power_dbfs", s.power_dbfs)
}

/// §5 `StationStatus`: finite floats, at most [`limits::MAX_RECEIVERS`]
/// receivers, each once (not 0) with a specified state, and a specified
/// state in every health sub-message present (§5.1). Whether each receiver is
/// configured is the session's check.
fn check_status(s: &v1::StationStatus, minor: u32) -> Result<(), ViolationKind> {
    if let Some(host) = &s.host {
        check_finite("status.host.load_avg_1m", host.load_avg_1m)?;
        if let Some(t) = host.cpu_temp_c {
            check_finite("status.host.cpu_temp_c", t)?;
        }
    }
    if s.receivers.len() > limits::MAX_RECEIVERS {
        return Err(ViolationKind::TooManyEntries {
            field: "status.receivers",
            len: s.receivers.len(),
            max: limits::MAX_RECEIVERS,
        });
    }
    let mut ids = HashSet::with_capacity(s.receivers.len());
    for rx in &s.receivers {
        if rx.receiver_id == 0 {
            return Err(ViolationKind::UnknownReceiver {
                kind: MessageKind::Status,
                receiver_id: 0,
            });
        }
        if !ids.insert(rx.receiver_id) {
            return Err(ViolationKind::DuplicateReceiver {
                field: "status.receivers.receiver_id",
                receiver_id: rx.receiver_id,
            });
        }
        check_receiver_health(rx, minor)?;
    }
    Ok(())
}

fn check_receiver_health(rx: &v1::ReceiverHealth, minor: u32) -> Result<(), ViolationKind> {
    check_enum::<v1::ReceiverState>("status.receivers.state", rx.state, minor, true)?;
    check_len(
        "status.receivers.detail",
        &rx.detail,
        limits::MAX_DETAIL_BYTES,
    )?;
    if let Some(b) = &rx.backend {
        check_enum::<v1::backend_health::State>(
            "status.receivers.backend.state",
            b.state,
            minor,
            true,
        )?;
        check_len(
            "status.receivers.backend.version",
            &b.version,
            limits::MAX_VERSION_BYTES,
        )?;
    }
    if let Some(f) = &rx.frontend {
        check_enum::<v1::frontend_health::State>(
            "status.receivers.frontend.state",
            f.state,
            minor,
            true,
        )?;
        check_finite("status.receivers.frontend.gain_db", f.gain_db)?;
        if let Some(p) = f.input_power_dbfs {
            check_finite("status.receivers.frontend.input_power_dbfs", p)?;
        }
    }
    if let Some(cpu) = rx.backend_cpu {
        check_finite("status.receivers.backend_cpu", cpu)?;
    }
    Ok(())
}

/// The event kinds that name a receiver and require `receiver_id` (§5.1;
/// `MEASUREMENT_DUTY` too, §5.2).
pub(crate) fn kind_requires_receiver(kind: i32) -> bool {
    use v1::event::Kind;
    matches!(
        Kind::try_from(kind),
        Ok(Kind::ReceiverState
            | Kind::DeviceLost
            | Kind::DeviceRestored
            | Kind::FrontendOverload
            | Kind::FrontendChanged
            | Kind::BackendRestarted
            | Kind::MeasurementDuty)
    )
}

fn check_event(e: &v1::Event, minor: u32) -> Result<(), ViolationKind> {
    use v1::event::Kind;
    check_enum::<Kind>("event.kind", e.kind, minor, true)?;
    check_len("event.detail", &e.detail, limits::MAX_DETAIL_BYTES)?;
    let kind = Kind::try_from(e.kind).unwrap_or(Kind::Unspecified);
    if kind == Kind::ChannelClosed && e.channel_id.is_none() {
        return Err(missing("event.channel_id"));
    }
    if kind == Kind::SubscriptionClosed && e.subscription_id.is_none() {
        return Err(missing("event.subscription_id"));
    }
    if kind_requires_receiver(e.kind) && e.receiver_id.is_none() {
        return Err(missing("event.receiver_id"));
    }
    if let Some(s) = e.receiver_state {
        check_enum::<v1::ReceiverState>("event.receiver_state", s, minor, true)?;
    } else if kind == Kind::ReceiverState {
        return Err(missing("event.receiver_state"));
    }
    if let Some(d) = e.measurement_duty {
        check_enum::<v1::MeasurementDuty>("event.measurement_duty", d, minor, true)?;
    }
    if let Some(r) = e.measurement_stop_reason {
        check_enum::<v1::MeasurementStopReason>("event.measurement_stop_reason", r, minor, true)?;
    }
    if kind == Kind::MeasurementStopped && e.measurement_id.is_none() {
        return Err(missing("event.measurement_id"));
    }
    if kind == Kind::MeasurementDuty {
        if e.measurement_id.is_none() {
            return Err(missing("event.measurement_id"));
        }
        if e.measurement_duty.is_none() {
            return Err(missing("event.measurement_duty"));
        }
        if e.effective_unix_ms.is_none() {
            return Err(missing("event.effective_unix_ms"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ helpers

pub(crate) fn missing(field: &'static str) -> ViolationKind {
    ViolationKind::MissingField { field }
}

pub(crate) fn bounds(field: &'static str) -> ViolationKind {
    ViolationKind::OutOfBounds { field }
}

/// §5 Strings. UTF-8 validity is already guaranteed by prost's decoder.
pub(crate) fn check_len(field: &'static str, s: &str, max: usize) -> Result<(), ViolationKind> {
    if s.len() > max {
        return Err(ViolationKind::StringTooLong {
            field,
            len: s.len(),
            max,
        });
    }
    Ok(())
}

pub(crate) fn check_finite(field: &'static str, v: f32) -> Result<(), ViolationKind> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(ViolationKind::NonFiniteFloat { field })
    }
}

fn check_frequency(field: &'static str, hz: u64) -> Result<(), ViolationKind> {
    if hz > limits::MAX_FREQUENCY_HZ {
        return Err(bounds(field));
    }
    Ok(())
}

fn check_cuts(field: &'static str, low: i32, high: i32) -> Result<(), ViolationKind> {
    if low > high {
        return Err(bounds(field));
    }
    Ok(())
}

/// A closed range that is ordered and lies at or below 6 GHz.
pub(crate) fn check_range(
    field: &'static str,
    r: &v1::FrequencyRange,
) -> Result<(), ViolationKind> {
    if r.start_hz > r.end_hz || r.end_hz > limits::MAX_FREQUENCY_HZ {
        return Err(ViolationKind::InvalidRange { field });
    }
    Ok(())
}

/// §5 Enums: `value` must be known in the negotiated `minor`, and must not be
/// `UNSPECIFIED` (0 in every enum of the package) when `required`. Every
/// value this crate knows belongs to 2.0, so `minor` only labels the error;
/// a later minor gates its new values here.
pub(crate) fn check_enum<E: TryFrom<i32>>(
    field: &'static str,
    value: i32,
    minor: u32,
    required: bool,
) -> Result<(), ViolationKind> {
    if !enum_known::<E>(value) {
        return Err(ViolationKind::UnknownEnumValue {
            field,
            value,
            minor,
        });
    }
    if required && value == 0 {
        return Err(ViolationKind::UnspecifiedEnum { field });
    }
    Ok(())
}

/// Whether `value` is a known value of `E` (the station-side counterpart of
/// [`check_enum`], §3).
pub(crate) fn enum_known<E: TryFrom<i32>>(value: i32) -> bool {
    E::try_from(value).is_ok()
}
