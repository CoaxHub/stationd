// SPDX-License-Identifier: MIT
//! Fuzzes protobuf decoding of `StationMessage`, the stateless validators and
//! the hub's `Session` (station-protocol §5, §10).
//!
//! Input: a sequence of frames `[len: u16 LE][step: u8][len bytes]`. Each
//! frame is decoded with the §5 size limit, checked statelessly, and fed to
//! two sessions: a fresh one (waiting for `Hello`) and a busy one with open
//! channels, subscriptions and one outstanding request of every kind. `step`
//! advances the injected clock by `step * 20` ms, so rate limits, grace
//! periods and response timeouts are all reachable. Nothing may panic.
#![no_main]

use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{Session, check_message, decode_station_message};

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

fn open(channel_id: u32) -> Command {
    Command::OpenChannel(v1::OpenChannel {
        channel_id,
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

/// Channels 1–3 (requests 1–3) and subscriptions 1–2 (requests 4–5) open;
/// requests 6–14 outstanding: `GetCapabilities`, `OpenChannel(4)`,
/// `UpdateChannel(1)`, `CloseChannel(3)`, `SetFrontend`,
/// `SubscribeWaterfall(3)`, `UnsubscribeWaterfall(2)`, `Ping(nonce 5)`,
/// `AdminOp`.
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
    for ch in 1..=3u32 {
        let id = u64::from(ch);
        let _ = s.on_request_sent(&request(id, open(ch)), t0);
        let opened = v1::ChannelOpened {
            channel_id: ch,
            frequency_hz: 7_100_000,
            mode: v1::Mode::Usb as i32,
            low_cut_hz: 200,
            high_cut_hz: 2800,
            audio: Some(audio()),
        };
        let _ = s.check(&ok(id, Payload::ChannelOpened(opened)), t0);
    }
    for sub in 1..=2u32 {
        let id = 3 + u64::from(sub);
        let _ = s.on_request_sent(&request(id, subscribe(sub)), t0);
        let subscribed = v1::WaterfallSubscribed {
            subscription_id: sub,
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
    let pending = [
        Command::GetCapabilities(v1::GetCapabilities {}),
        open(4),
        Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 1,
            ..Default::default()
        }),
        Command::CloseChannel(v1::CloseChannel { channel_id: 3 }),
        Command::SetFrontend(v1::SetFrontend::default()),
        subscribe(3),
        Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 2 }),
        Command::Ping(v1::Ping {
            nonce: 5,
            hub_time_unix_ms: 0,
        }),
        Command::AdminOp(v1::AdminOp { kind: 1 }),
    ];
    for (i, c) in pending.into_iter().enumerate() {
        let _ = s.on_request_sent(&request(6 + i as u64, c), t0);
    }
    s
}

fuzz_target!(|data: &[u8]| {
    let t0 = Instant::now();
    // The whole input as one message too.
    if let Ok(m) = decode_station_message(data, usize::MAX) {
        let _ = check_message(&m, 0);
    }
    let mut fresh = Session::new(t0);
    let mut busy = busy_session(t0);
    let mut now = t0;
    let mut rest = data;
    while rest.len() >= 3 {
        let len = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
        let step = u64::from(rest[2]);
        rest = &rest[3..];
        let frame = &rest[..len.min(rest.len())];
        rest = &rest[frame.len()..];
        now += Duration::from_millis(step * 20);
        let Ok(m) = decode_station_message(frame, usize::MAX) else {
            continue;
        };
        let stateless = check_message(&m, 0);
        let _ = fresh.check(&m, now);
        let verdict = busy.check(&m, now);
        // Every stateless rejection is a violation in an established session.
        if stateless.is_err() && !matches!(m.body, Some(Body::Hello(_))) {
            assert!(matches!(
                verdict,
                station_proto::validate::Verdict::Violation(_)
            ));
        }
        let _ = busy.expire_requests(now);
        let _ = busy.next_deadline();
        assert!(busy.last_waterfall_row().len() <= 16_384);
    }
});
