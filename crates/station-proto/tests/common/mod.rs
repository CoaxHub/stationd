// SPDX-License-Identifier: MIT
//! Builders shared by the integration tests.
#![allow(dead_code, clippy::missing_panics_doc)]

use std::time::{Duration, Instant};

use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{Session, Verdict};

pub const OK: i32 = v1::ResultCode::Ok as i32;
pub const NOT_FOUND: i32 = v1::ResultCode::NotFound as i32;
pub const ZSTD: i32 = v1::WaterfallCompression::Zstd as i32;

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub fn msg(body: Body) -> v1::StationMessage {
    v1::StationMessage { body: Some(body) }
}

pub fn hello_with(major: u32, minor: u32) -> v1::Hello {
    v1::Hello {
        protocol_version: Some(v1::ProtocolVersion { major, minor }),
        daemon_version: "0.1.0".into(),
        backend: Some(v1::BackendInfo {
            kind: v1::BackendKind::Ka9qRadio as i32,
            version: "ka9q 2026.09".into(),
        }),
        platform: "linux-aarch64".into(),
        boot_id: 42,
    }
}

pub fn hello() -> v1::StationMessage {
    msg(Body::Hello(hello_with(1, 0)))
}

/// A session that accepted `Hello` at `t0` (protocol 1.0).
pub fn established(t0: Instant) -> Session {
    let mut s = Session::new(t0);
    assert_eq!(s.check(&hello(), t0), Verdict::Accept);
    s
}

/// A session that accepted a 1.1 `Hello` at `t0` (measurements).
pub fn established_1_1(t0: Instant) -> Session {
    let mut s = Session::new(t0);
    assert_eq!(
        s.check(&msg(Body::Hello(hello_with(1, 1))), t0),
        Verdict::Accept
    );
    s
}

pub fn request(id: u64, command: Command) -> v1::Request {
    v1::Request {
        request_id: id,
        command: Some(command),
    }
}

pub fn audio_params(frame_duration_ms: u32) -> v1::AudioParams {
    v1::AudioParams {
        codec: v1::AudioCodec::Opus as i32,
        bitrate_bps: 24_000,
        sample_rate_hz: 12_000,
        frame_duration_ms,
    }
}

pub fn open_channel(channel_id: u32, frame_duration_ms: u32) -> Command {
    Command::OpenChannel(v1::OpenChannel {
        channel_id,
        frequency_hz: 7_100_000,
        mode: v1::Mode::Lsb as i32,
        low_cut_hz: -2800,
        high_cut_hz: -200,
        audio: Some(audio_params(frame_duration_ms)),
        squelch_snr_db: None,
    })
}

pub fn close_channel(channel_id: u32) -> Command {
    Command::CloseChannel(v1::CloseChannel { channel_id })
}

pub fn range(start_hz: u64, end_hz: u64) -> v1::FrequencyRange {
    v1::FrequencyRange { start_hz, end_hz }
}

pub fn subscribe(subscription_id: u32, bins: u32, row_interval_ms: u32) -> Command {
    Command::SubscribeWaterfall(v1::SubscribeWaterfall {
        subscription_id,
        range: Some(range(7_000_000, 7_300_000)),
        bins,
        row_interval_ms,
        quantization: Some(quantization()),
        compression: ZSTD,
    })
}

pub fn unsubscribe(subscription_id: u32) -> Command {
    Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id })
}

pub fn quantization() -> v1::Quantization {
    v1::Quantization {
        offset_db: -130.0,
        step_db: 0.5,
    }
}

pub fn response(request_id: u64, code: i32, payload: Option<Payload>) -> v1::StationMessage {
    msg(Body::Response(v1::Response {
        request_id,
        result: Some(v1::Result {
            code,
            message: String::new(),
        }),
        payload,
    }))
}

pub fn capabilities() -> v1::Capabilities {
    v1::Capabilities {
        tunable_range: Some(range(0, 2_400_000)),
        hardware_range: Some(range(0, 1_766_000_000)),
        sample_rate_hz: 2_400_000,
        max_channels: 16,
        modes: vec![1, 2, 3, 4, 5, 6],
        codecs: vec![1],
        waterfall: Some(v1::WaterfallLimits {
            max_bins: 4096,
            min_row_interval_ms: 50,
            max_subscriptions: 4,
        }),
        frontend: Some(v1::FrontendControls {
            has_gain: true,
            min_gain_db: 0.0,
            max_gain_db: 49.6,
            has_hardware_agc: true,
            has_attenuator: false,
            max_attenuation_db: 0.0,
            antenna_ports: vec!["A".into(), "B".into()],
            can_retune: true,
        }),
        allowed_admin_ops: vec![1],
        frontend_control_allowed: true,
        sdr_model: "RTL-SDR Blog V4".into(),
        measurements: None,
    }
}

pub fn channel_opened(channel_id: u32, frame_duration_ms: u32) -> Payload {
    Payload::ChannelOpened(v1::ChannelOpened {
        channel_id,
        frequency_hz: 7_100_000,
        mode: v1::Mode::Lsb as i32,
        low_cut_hz: -2800,
        high_cut_hz: -200,
        audio: Some(audio_params(frame_duration_ms)),
    })
}

pub fn channel_updated(channel_id: u32) -> Payload {
    Payload::ChannelUpdated(v1::ChannelUpdated {
        channel_id,
        frequency_hz: 7_150_000,
        mode: v1::Mode::Usb as i32,
        low_cut_hz: 200,
        high_cut_hz: 2800,
    })
}

pub fn frontend_state() -> Payload {
    Payload::FrontendState(v1::FrontendState {
        gain_db: 30.0,
        hardware_agc: false,
        attenuation_db: 0.0,
        antenna_port: "A".into(),
        center_frequency_hz: 7_100_000,
        tunable_range: Some(range(5_900_000, 8_300_000)),
    })
}

pub fn waterfall_subscribed(subscription_id: u32, bins: u32, row_interval_ms: u32) -> Payload {
    Payload::WaterfallSubscribed(v1::WaterfallSubscribed {
        subscription_id,
        range: Some(range(7_000_000, 7_300_000)),
        bins,
        row_interval_ms,
        quantization: Some(quantization()),
        compression: ZSTD,
    })
}

pub fn pong(nonce: u64) -> Payload {
    Payload::Pong(v1::Pong {
        nonce,
        station_time_unix_ms: 0,
    })
}

pub fn audio(channel_id: u32, payload_len: usize) -> v1::StationMessage {
    msg(Body::Audio(v1::AudioFrame {
        channel_id,
        seq: 0,
        timestamp: 0,
        payload: vec![0xfc; payload_len],
    }))
}

pub fn row_bytes(bins: usize) -> Vec<u8> {
    (0..bins)
        .map(|i| u8::try_from(i % 200).unwrap_or(0))
        .collect()
}

pub fn waterfall(subscription_id: u32, payload: Vec<u8>) -> v1::StationMessage {
    msg(Body::Waterfall(v1::WaterfallRow {
        subscription_id,
        seq: 0,
        timestamp_us: 0,
        payload,
    }))
}

pub fn zstd_row(bins: usize) -> Vec<u8> {
    zstd::bulk::compress(&row_bytes(bins), 3).expect("compress")
}

pub fn signal(channel_id: u32, snr_db: f32) -> v1::StationMessage {
    msg(Body::Signal(v1::ChannelSignal {
        channel_id,
        snr_db,
        power_dbfs: -40.0,
        squelch_open: None,
    }))
}

pub fn status() -> v1::StationMessage {
    msg(Body::Status(v1::StationStatus {
        uptime_s: 100,
        host: Some(v1::HostHealth {
            load_avg_1m: 0.5,
            cpu_count: 4,
            cpu_temp_c: Some(48.0),
            mem_used_bytes: 1,
            mem_total_bytes: 2,
            host_uptime_s: 3,
        }),
        backend: Some(v1::BackendHealth {
            state: v1::backend_health::State::Running as i32,
            version: "x".into(),
        }),
        frontend: Some(v1::FrontendHealth {
            state: v1::frontend_health::State::Ok as i32,
            sample_rate_hz: 2_400_000,
            center_frequency_hz: 7_100_000,
            gain_db: 30.0,
            overloads_total: 0,
            input_power_dbfs: Some(-30.0),
        }),
        ..Default::default()
    }))
}

pub fn event(
    kind: v1::event::Kind,
    channel_id: Option<u32>,
    subscription_id: Option<u32>,
) -> v1::StationMessage {
    msg(Body::Event(v1::Event {
        kind: kind as i32,
        channel_id,
        subscription_id,
        detail: String::new(),
        ..Default::default()
    }))
}

/// Established session with channel `channel_id` opened by request 1 and
/// confirmed (`frame_duration_ms` 20).
pub fn with_channel(t0: Instant, channel_id: u32) -> Session {
    let mut s = established(t0);
    s.on_request_sent(&request(1, open_channel(channel_id, 20)), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(1, OK, Some(channel_opened(channel_id, 20))), t0),
        Verdict::Accept
    );
    s
}

/// Established session with subscription `id` (bins, row interval) opened by
/// request 1 and confirmed.
pub fn with_subscription(t0: Instant, id: u32, bins: u32, row_interval_ms: u32) -> Session {
    let mut s = established(t0);
    s.on_request_sent(&request(1, subscribe(id, bins, row_interval_ms)), t0)
        .expect("request");
    assert_eq!(
        s.check(
            &response(1, OK, Some(waterfall_subscribed(id, bins, row_interval_ms))),
            t0
        ),
        Verdict::Accept
    );
    s
}

pub fn is_violation(v: &Verdict) -> bool {
    matches!(v, Verdict::Violation(_))
}

// ------------------------------------------------------------------ measurements (1.1)

/// 40 m as one band (id 1, two pieces around a DC guard) and 20 m (id 2).
pub fn start_measurements(measurement_id: u32) -> Command {
    Command::StartMeasurements(v1::StartMeasurements {
        measurement_id,
        noise_bands: vec![
            v1::NoiseBand {
                band_id: 1,
                ranges: vec![range(7_000_000, 7_098_000), range(7_102_000, 7_300_000)],
                key: "40m".into(),
            },
            v1::NoiseBand {
                band_id: 2,
                ranges: vec![range(14_000_000, 14_350_000)],
                key: "20m".into(),
            },
        ],
        noise_interval_ms: 60_000,
        bin_count: 1024,
        resolution_bw_hz: 500.0,
        dwell_ms: 4000,
        poll_interval_ms: 500,
    })
}

/// Accepts band 1 of [`start_measurements`] only, under idle duty.
pub fn measurements_started(measurement_id: u32) -> Payload {
    Payload::MeasurementsStarted(v1::MeasurementsStarted {
        measurement_id,
        noise_bands: vec![v1::NoiseBand {
            band_id: 1,
            ranges: vec![range(7_000_000, 7_098_000), range(7_102_000, 7_300_000)],
            key: "40m".into(),
        }],
        noise_interval_ms: 60_000,
        duty: v1::MeasurementDuty::Idle as i32,
        frontend: Some(v1::FrontendState {
            gain_db: 20.0,
            center_frequency_hz: 7_100_000,
            tunable_range: Some(range(6_076_000, 8_124_000)),
            ..Default::default()
        }),
        method_version: 1,
    })
}

/// A valid report for band 1 of measurement `measurement_id`, window at
/// `window_start_unix_ms`.
pub fn noise(measurement_id: u32, window_start_unix_ms: i64) -> v1::StationMessage {
    msg(Body::Noise(v1::NoiseFloorReport {
        measurement_id,
        window_start_unix_ms,
        window_ms: 60_000,
        frontend: Some(v1::FrontendState {
            gain_db: 20.0,
            center_frequency_hz: 7_100_000,
            tunable_range: Some(range(6_076_000, 8_124_000)),
            ..Default::default()
        }),
        overloads: 0,
        clock: Some(v1::ClockState {
            synchronized: true,
            max_error_ms: 5,
        }),
        bands: vec![v1::BandNoise {
            band_id: 1,
            measured: Some(range(7_000_000, 7_300_000)),
            resolution_bw_hz: 500.0,
            samples: 72_000,
            p10_db_hz: -132.5,
            p25_db_hz: -131.0,
            p50_db_hz: -129.9,
            p90_db_hz: -110.0,
        }],
        partial: false,
        cycle_minutes: 1,
        cycle_index: 0,
    }))
}

/// `Event{MEASUREMENT_DUTY}` for `measurement_id`.
pub fn duty_event(
    measurement_id: u32,
    duty: v1::MeasurementDuty,
    effective_unix_ms: i64,
) -> v1::StationMessage {
    msg(Body::Event(v1::Event {
        kind: v1::event::Kind::MeasurementDuty as i32,
        measurement_id: Some(measurement_id),
        measurement_duty: Some(duty as i32),
        effective_unix_ms: Some(effective_unix_ms),
        ..Default::default()
    }))
}

/// `Event{MEASUREMENT_STOPPED}` for `measurement_id`.
pub fn stopped_event(measurement_id: u32) -> v1::StationMessage {
    msg(Body::Event(v1::Event {
        kind: v1::event::Kind::MeasurementStopped as i32,
        measurement_id: Some(measurement_id),
        measurement_stop_reason: Some(v1::MeasurementStopReason::FrontendChanged as i32),
        ..Default::default()
    }))
}

/// A 1.1 session with plan `measurement_id` started (request 1) and accepted.
pub fn with_plan(t0: Instant, measurement_id: u32) -> Session {
    let mut s = established_1_1(t0);
    s.on_request_sent(&request(1, start_measurements(measurement_id)), t0)
        .expect("request");
    assert_eq!(
        s.check(
            &response(1, OK, Some(measurements_started(measurement_id))),
            t0
        ),
        Verdict::Accept
    );
    s
}
