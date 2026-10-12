// SPDX-License-Identifier: MIT
//! Fuzzes protobuf decoding of `StationMessage`, the stateless validators and
//! the hub's `Session` (station-protocol §5, §10).
//!
//! Input: a sequence of frames `[len: u16 LE][step: u8][len bytes]`. Each
//! frame is decoded with the §5 size limit, checked statelessly, and fed to
//! two sessions: a fresh one (waiting for `Hello`) and a busy one with
//! receivers 1 and 2 configured, open channels, subscriptions, a measurement
//! plan and one outstanding request of every kind. `step` advances the
//! injected clock and the hub's wall clock by `step * 20` ms, so rate limits,
//! grace periods, response timeouts and the §5.2 time windows are all
//! reachable. A decoded `Hardware` is also checked against a receiver set
//! (§4.2) and a `Capabilities` run through the hub's receiver selection
//! (§5.1). Nothing may panic.
#![no_main]

use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{
    Session, check_message, check_receiver_set, decode_station_message, select_receivers,
};

fn request(id: u64, command: Command) -> v1::Request {
    v1::Request {
        request_id: id,
        command: Some(command),
    }
}

fn ok(request_id: u64, payload: Payload) -> v1::StationMessage {
    v1::StationMessage {
        body: Some(Body::Response(v1::Response {
            request_id,
            result: Some(v1::Result {
                code: v1::ResultCode::Ok as i32,
                message: String::new(),
            }),
            payload: Some(payload),
        })),
    }
}

fn configure(ids: &[u32]) -> Command {
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

fn configured(ids: &[u32]) -> Payload {
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

fn open(channel_id: u32) -> Command {
    Command::OpenChannel(v1::OpenChannel {
        channel_id,
        receiver_id: 1,
        frequency_hz: 7_100_000,
        mode: v1::Mode::Usb as i32,
        low_cut_hz: 200,
        high_cut_hz: 2800,
        audio: Some(audio()),
        squelch_snr_db: None,
    })
}

fn audio() -> v1::AudioParams {
    v1::AudioParams {
        codec: 1,
        bitrate_bps: 24_000,
        sample_rate_hz: 12_000,
        frame_duration_ms: 20,
    }
}

fn range() -> v1::FrequencyRange {
    v1::FrequencyRange {
        start_hz: 7_000_000,
        end_hz: 7_300_000,
    }
}

fn subscribe(subscription_id: u32) -> Command {
    Command::SubscribeWaterfall(v1::SubscribeWaterfall {
        subscription_id,
        receiver_id: 1,
        range: Some(range()),
        bins: 256,
        row_interval_ms: 100,
        quantization: Some(v1::Quantization {
            offset_db: -130.0,
            step_db: 0.5,
        }),
        compression: v1::WaterfallCompression::Zstd as i32,
    })
}

fn band(band_id: u32, receiver_id: u32, start_hz: u64, end_hz: u64) -> v1::NoiseBand {
    v1::NoiseBand {
        band_id,
        receiver_id,
        ranges: vec![v1::FrequencyRange { start_hz, end_hz }],
        key: "40m".into(),
    }
}

fn start(measurement_id: u32) -> Command {
    Command::StartMeasurements(v1::StartMeasurements {
        measurement_id,
        noise_bands: vec![
            band(1, 1, 7_000_000, 7_300_000),
            band(2, 2, 14_000_000, 14_350_000),
        ],
        noise_interval_ms: 60_000,
        bin_count: 1024,
        resolution_bw_hz: 500.0,
        dwell_ms: 4000,
        poll_interval_ms: 500,
    })
}

/// Receivers 1 and 2 configured (request 1). Channels 1–3 on receiver 1
/// (requests 2–4), subscriptions 1–2 (requests 5–6) and plan 1 (request 7,
/// receiver 1 accepted idle) open; requests 8–20 outstanding:
/// `GetCapabilities`, `OpenChannel(4)`, `UpdateChannel(1)`, `CloseChannel(3)`,
/// `SetFrontend(1)`, `SubscribeWaterfall(3)`, `UnsubscribeWaterfall(2)`,
/// `Ping(nonce 5)`, `AdminOp`, `StartMeasurements(2)`, `StopMeasurements(1)`,
/// `GetHardware`, `ConfigureReceivers(1, 2, 3)`.
fn busy_session(t0: Instant) -> Session {
    let mut s = Session::new(t0);
    let hello = v1::StationMessage {
        body: Some(Body::Hello(v1::Hello {
            protocol_version: Some(station_proto::PROTOCOL_VERSION),
            backend: Some(v1::BackendInfo {
                kind: 1,
                version: String::new(),
            }),
            ..Default::default()
        })),
    };
    let _ = s.check(&hello, t0);
    let _ = s.on_request_sent(&request(1, configure(&[1, 2])), t0);
    let _ = s.check(&ok(1, configured(&[1, 2])), t0);
    for ch in 1..=3u32 {
        let id = 1 + u64::from(ch);
        let _ = s.on_request_sent(&request(id, open(ch)), t0);
        let opened = v1::ChannelOpened {
            channel_id: ch,
            receiver_id: 1,
            frequency_hz: 7_100_000,
            mode: v1::Mode::Usb as i32,
            low_cut_hz: 200,
            high_cut_hz: 2800,
            audio: Some(audio()),
        };
        let _ = s.check(&ok(id, Payload::ChannelOpened(opened)), t0);
    }
    for sub in 1..=2u32 {
        let id = 4 + u64::from(sub);
        let _ = s.on_request_sent(&request(id, subscribe(sub)), t0);
        let subscribed = v1::WaterfallSubscribed {
            subscription_id: sub,
            receiver_id: 1,
            range: Some(range()),
            bins: 256,
            row_interval_ms: 100,
            quantization: Some(v1::Quantization {
                offset_db: -130.0,
                step_db: 0.5,
            }),
            compression: v1::WaterfallCompression::Zstd as i32,
        };
        let _ = s.check(&ok(id, Payload::WaterfallSubscribed(subscribed)), t0);
    }
    let _ = s.on_request_sent(&request(7, start(1)), t0);
    let started = v1::MeasurementsStarted {
        measurement_id: 1,
        noise_bands: vec![band(1, 1, 7_000_000, 7_300_000)],
        receivers: vec![v1::MeasuredReceiver {
            receiver_id: 1,
            noise_interval_ms: 60_000,
            duty: v1::MeasurementDuty::Idle as i32,
            frontend: Some(v1::FrontendState {
                receiver_id: 1,
                ..Default::default()
            }),
        }],
        method_version: 1,
    };
    let _ = s.check(&ok(7, Payload::MeasurementsStarted(started)), t0);
    let pending = [
        Command::GetCapabilities(v1::GetCapabilities {}),
        open(4),
        Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 1,
            ..Default::default()
        }),
        Command::CloseChannel(v1::CloseChannel { channel_id: 3 }),
        Command::SetFrontend(v1::SetFrontend {
            receiver_id: 1,
            ..Default::default()
        }),
        subscribe(3),
        Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 2 }),
        Command::Ping(v1::Ping {
            nonce: 5,
            hub_time_unix_ms: 0,
        }),
        Command::AdminOp(v1::AdminOp {
            kind: 2,
            receiver_id: Some(1),
        }),
        start(2),
        Command::StopMeasurements(v1::StopMeasurements { measurement_id: 1 }),
        Command::GetHardware(v1::GetHardware {}),
        configure(&[1, 2, 3]),
    ];
    for (i, c) in pending.into_iter().enumerate() {
        let _ = s.on_request_sent(&request(8 + i as u64, c), t0);
    }
    s
}

/// The hub's wall clock at the fuzzer's `t0`: 2026-10-02 12:00 UTC, a
/// measuring minute.
const T0_UNIX_MS: i64 = 1_790_942_400_000;

fuzz_target!(|data: &[u8]| {
    let t0 = Instant::now();
    // The whole input as one message too.
    if let Ok(m) = decode_station_message(data, usize::MAX) {
        let _ = check_message(&m, 0);
        if let Some(Body::Response(r)) = &m.body {
            match &r.payload {
                Some(Payload::Hardware(h)) => {
                    if let Command::ConfigureReceivers(set) = configure(&[1, 2]) {
                        let _ = check_receiver_set(&set, h);
                    }
                }
                Some(Payload::Capabilities(c)) => {
                    let _ = select_receivers(&c.receivers, 6);
                }
                _ => {}
            }
        }
    }
    let mut fresh = Session::new(t0);
    let mut busy = busy_session(t0);
    let mut now = t0;
    let mut unix_ms = T0_UNIX_MS;
    let mut rest = data;
    while rest.len() >= 3 {
        let len = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
        let step = u64::from(rest[2]);
        rest = &rest[3..];
        let frame = &rest[..len.min(rest.len())];
        rest = &rest[frame.len()..];
        now += Duration::from_millis(step * 20);
        unix_ms += i64::try_from(step * 20).unwrap_or(0);
        let Ok(m) = decode_station_message(frame, usize::MAX) else {
            continue;
        };
        let stateless = check_message(&m, station_proto::PROTOCOL_VERSION.minor);
        let _ = fresh.check(&m, now);
        let verdict = busy.check_at(&m, now, Some(unix_ms));
        // Every stateless rejection is a violation in an established session.
        if stateless.is_err() && !matches!(m.body, Some(Body::Hello(_))) {
            assert!(matches!(
                verdict,
                station_proto::validate::Verdict::Violation(_)
            ));
        }
        let _ = busy.expire_requests(now);
        let _ = busy.next_deadline();
        let _ = busy.running_receivers();
        assert!(busy.last_waterfall_row().len() <= 16_384);
    }
});
