// SPDX-License-Identifier: MIT
//! Builders shared by the integration tests.
#![allow(dead_code, clippy::missing_panics_doc)]

use std::time::{Duration, Instant};

use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{Session, Verdict};

pub const OK: i32 = v1::ResultCode::Ok as i32;
pub const NOT_FOUND: i32 = v1::ResultCode::NotFound as i32;
pub const ZSTD: i32 = v1::WaterfallCompression::Zstd as i32;

/// The receiver the builders use: 40 m, centred on 7.1 MHz.
pub const RX: u32 = 1;
/// A second receiver: 20 m, centred on 14.2 MHz.
pub const RX2: u32 = 2;

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
    msg(Body::Hello(hello_with(2, 0)))
}

/// A session that accepted `Hello` at `t0` (protocol 2.0). No receiver set
/// sent yet.
pub fn established(t0: Instant) -> Session {
    let mut s = Session::new(t0);
    assert_eq!(s.check(&hello(), t0), Verdict::Accept);
    s
}

/// `ConfigureReceivers` with one receiver per id, each on its own band.
pub fn configure(ids: &[u32]) -> Command {
    Command::ConfigureReceivers(v1::ConfigureReceivers {
        receivers: ids
            .iter()
            .map(|&receiver_id| v1::ReceiverConfig {
                receiver_id,
                sdr_serial: format!("{receiver_id:08}"),
                model: v1::SdrModel::RtlsdrDirectSampling as i32,
                profile: v1::SdrProfile::RtlsdrHf as i32,
                center_frequency_hz: 7_100_000 * u64::from(receiver_id),
                ..Default::default()
            })
            .collect(),
    })
}

/// An SDR the station reports, offering `models` (each with its profiles).
pub fn sdr(serial: &str, product: &str, models: &[(v1::SdrModel, &[v1::SdrProfile])]) -> v1::Sdr {
    v1::Sdr {
        usb_vendor_id: 0x0bda,
        usb_product_id: 0x2838,
        manufacturer: "RTLSDRBlog".into(),
        product: product.into(),
        serial: serial.into(),
        port: "1-1.3".into(),
        models: models
            .iter()
            .map(|(model, profiles)| v1::ModelOption {
                model: *model as i32,
                profiles: profiles.iter().map(|p| *p as i32).collect(),
            })
            .collect(),
        receiver_id: None,
    }
}

/// What the package's profiles can do (`stationd`'s numbers).
pub fn profile_info(profile: v1::SdrProfile) -> v1::ProfileInfo {
    use v1::SdrProfile as P;
    let tuner = v1::FrontendControls {
        has_gain: true,
        min_gain_db: 0.0,
        max_gain_db: 49.6,
        has_hardware_agc: true,
        can_retune: true,
        ..Default::default()
    };
    let (center, default_center_hz, frontend) = match profile {
        P::RtlsdrHf => (
            range(1_024_000, 28_800_000),
            7_100_000,
            v1::FrontendControls {
                can_retune: true,
                ..Default::default()
            },
        ),
        P::RtlsdrV4 => (range(500_000, 1_766_000_000), 7_100_000, tuner),
        P::Rtlsdr | P::Unspecified => (range(24_000_000, 1_766_000_000), 98_000_000, tuner),
    };
    v1::ProfileInfo {
        profile: profile as i32,
        center_range: Some(center),
        default_center_hz,
        sample_rate_hz: 2_048_000,
        tunable_span_hz: 2_048_000,
        frontend: Some(frontend),
    }
}

/// Two RTL-SDR Blog V3s (serials `00000001` and `00000002`) and a V4
/// (`00000003`), as `stationd`'s table offers them.
pub fn hardware() -> v1::Hardware {
    use v1::{SdrModel as M, SdrProfile as P};
    let v3: &[(M, &[P])] = &[
        (M::RtlsdrDirectSampling, &[P::Rtlsdr, P::RtlsdrHf]),
        (M::RtlsdrOther, &[P::Rtlsdr]),
    ];
    v1::Hardware {
        sdrs: vec![
            sdr("00000001", "RTL2838UHIDIR", v3),
            sdr("00000002", "RTL2838UHIDIR", v3),
            sdr("00000003", "Blog V4", &[(M::RtlsdrBlogV4, &[P::RtlsdrV4])]),
        ],
        profiles: vec![
            profile_info(P::Rtlsdr),
            profile_info(P::RtlsdrHf),
            profile_info(P::RtlsdrV4),
        ],
    }
}

/// `ReceiversConfigured` for `ids`, all starting.
pub fn receivers_configured(ids: &[u32]) -> Payload {
    Payload::ReceiversConfigured(v1::ReceiversConfigured {
        receivers: ids
            .iter()
            .map(|&receiver_id| v1::ReceiverStatus {
                receiver_id,
                state: v1::ReceiverState::Starting as i32,
                detail: String::new(),
            })
            .collect(),
    })
}

/// A session with receivers `ids` configured by request 1 and accepted.
/// Further requests start at 2.
pub fn configured(t0: Instant, ids: &[u32]) -> Session {
    let mut s = established(t0);
    s.on_request_sent(&request(1, configure(ids)), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(1, OK, Some(receivers_configured(ids))), t0),
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
    open_channel_on(RX, channel_id, frame_duration_ms)
}

pub fn open_channel_on(receiver_id: u32, channel_id: u32, frame_duration_ms: u32) -> Command {
    Command::OpenChannel(v1::OpenChannel {
        channel_id,
        receiver_id,
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
    subscribe_on(RX, subscription_id, bins, row_interval_ms)
}

pub fn subscribe_on(
    receiver_id: u32,
    subscription_id: u32,
    bins: u32,
    row_interval_ms: u32,
) -> Command {
    Command::SubscribeWaterfall(v1::SubscribeWaterfall {
        subscription_id,
        receiver_id,
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

/// A running receiver: `receiver_id` with a 2.4 MHz tunable range from
/// `start_hz`.
pub fn receiver_capabilities(receiver_id: u32, start_hz: u64) -> v1::ReceiverCapabilities {
    v1::ReceiverCapabilities {
        receiver_id,
        tunable_range: Some(range(start_hz, start_hz + 2_400_000)),
        hardware_range: Some(range(0, 1_766_000_000)),
        sample_rate_hz: 2_400_000,
        max_channels: 16,
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
        sdr_model: "RTL-SDR Blog V4".into(),
    }
}

/// Receiver [`RX`] running on 5.9–8.3 MHz.
pub fn capabilities() -> v1::Capabilities {
    v1::Capabilities {
        receivers: vec![receiver_capabilities(RX, 5_900_000)],
        modes: vec![1, 2, 3, 4, 5, 6],
        codecs: vec![1],
        waterfall: Some(v1::WaterfallLimits {
            max_bins: 4096,
            min_row_interval_ms: 50,
            max_subscriptions: 4,
        }),
        measurements: None,
    }
}

pub fn channel_opened(channel_id: u32, frame_duration_ms: u32) -> Payload {
    Payload::ChannelOpened(v1::ChannelOpened {
        channel_id,
        receiver_id: RX,
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
        receiver_id: RX,
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
        receiver_id: RX,
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

/// The health of a running receiver.
pub fn receiver_health(receiver_id: u32) -> v1::ReceiverHealth {
    v1::ReceiverHealth {
        receiver_id,
        state: v1::ReceiverState::Running as i32,
        detail: String::new(),
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
        active_channels: 0,
        backend_cpu: Some(0.25),
    }
}

/// A status reporting `receivers`, each running.
pub fn status_of(receivers: &[u32]) -> v1::StationMessage {
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
        receivers: receivers.iter().map(|&id| receiver_health(id)).collect(),
        ..Default::default()
    }))
}

/// A status reporting receiver [`RX`].
pub fn status() -> v1::StationMessage {
    status_of(&[RX])
}

/// An event; kinds that name a receiver name [`RX`] (and `RECEIVER_STATE`
/// says it runs).
pub fn event(
    kind: v1::event::Kind,
    channel_id: Option<u32>,
    subscription_id: Option<u32>,
) -> v1::StationMessage {
    use v1::event::Kind;
    let receiver = matches!(
        kind,
        Kind::ReceiverState
            | Kind::DeviceLost
            | Kind::DeviceRestored
            | Kind::FrontendOverload
            | Kind::FrontendChanged
            | Kind::BackendRestarted
    );
    msg(Body::Event(v1::Event {
        kind: kind as i32,
        channel_id,
        subscription_id,
        detail: String::new(),
        receiver_id: receiver.then_some(RX),
        receiver_state: (kind == Kind::ReceiverState).then_some(v1::ReceiverState::Running as i32),
        ..Default::default()
    }))
}

/// An event of `kind` about receiver `receiver_id`.
pub fn receiver_event(kind: v1::event::Kind, receiver_id: u32) -> v1::StationMessage {
    let mut m = event(kind, None, None);
    if let Some(Body::Event(e)) = m.body.as_mut() {
        e.receiver_id = Some(receiver_id);
    }
    m
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

// ------------------------------------------------------------------ measurements

/// 40 m on [`RX`] as one band (id 1, two pieces around a DC guard) and 20 m
/// on [`RX2`] (id 2).
pub fn start_measurements(measurement_id: u32) -> Command {
    Command::StartMeasurements(v1::StartMeasurements {
        measurement_id,
        noise_bands: vec![
            v1::NoiseBand {
                band_id: 1,
                receiver_id: RX,
                ranges: vec![range(7_000_000, 7_098_000), range(7_102_000, 7_300_000)],
                key: "40m".into(),
            },
            v1::NoiseBand {
                band_id: 2,
                receiver_id: RX2,
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

/// The front end of [`RX`] while measuring.
pub fn measuring_frontend(receiver_id: u32) -> v1::FrontendState {
    v1::FrontendState {
        receiver_id,
        gain_db: 20.0,
        center_frequency_hz: 7_100_000,
        tunable_range: Some(range(6_076_000, 8_124_000)),
        ..Default::default()
    }
}

/// A receiver of a running plan, under `duty`, every minute.
pub fn measured(receiver_id: u32, duty: v1::MeasurementDuty) -> v1::MeasuredReceiver {
    v1::MeasuredReceiver {
        receiver_id,
        noise_interval_ms: 60_000,
        duty: duty as i32,
        frontend: Some(measuring_frontend(receiver_id)),
    }
}

/// Accepts band 1 of [`start_measurements`] only ([`RX`]), under idle duty.
pub fn measurements_started(measurement_id: u32) -> Payload {
    Payload::MeasurementsStarted(v1::MeasurementsStarted {
        measurement_id,
        noise_bands: vec![v1::NoiseBand {
            band_id: 1,
            receiver_id: RX,
            ranges: vec![range(7_000_000, 7_098_000), range(7_102_000, 7_300_000)],
            key: "40m".into(),
        }],
        receivers: vec![measured(RX, v1::MeasurementDuty::Idle)],
        method_version: 1,
    })
}

/// A valid report for band 1 ([`RX`]) of measurement `measurement_id`,
/// window at `window_start_unix_ms`.
pub fn noise(measurement_id: u32, window_start_unix_ms: i64) -> v1::StationMessage {
    msg(Body::Noise(v1::NoiseFloorReport {
        measurement_id,
        receiver_id: RX,
        window_start_unix_ms,
        window_ms: 60_000,
        frontend: Some(measuring_frontend(RX)),
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

/// `Event{MEASUREMENT_DUTY}` for [`RX`] in `measurement_id`.
pub fn duty_event(
    measurement_id: u32,
    duty: v1::MeasurementDuty,
    effective_unix_ms: i64,
) -> v1::StationMessage {
    duty_event_for(RX, measurement_id, duty, effective_unix_ms)
}

/// `Event{MEASUREMENT_DUTY}` for `receiver_id` in `measurement_id`.
pub fn duty_event_for(
    receiver_id: u32,
    measurement_id: u32,
    duty: v1::MeasurementDuty,
    effective_unix_ms: i64,
) -> v1::StationMessage {
    msg(Body::Event(v1::Event {
        kind: v1::event::Kind::MeasurementDuty as i32,
        receiver_id: Some(receiver_id),
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

/// A session with plan `measurement_id` started (request 1) and accepted.
pub fn with_plan(t0: Instant, measurement_id: u32) -> Session {
    let mut s = established(t0);
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
