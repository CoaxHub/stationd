// SPDX-License-Identifier: MIT
//! Property tests: the validators never panic on arbitrary messages, every
//! stateless rejection is also a session violation, generated messages
//! round-trip through the codec, and the backoff stays in bounds.

// Test tables are long by nature.
#![allow(clippy::too_many_lines, clippy::similar_names, clippy::type_complexity)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use proptest::prelude::*;
use prost::Message as _;
use station_proto::backoff::{Backoff, Decision};
use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{
    Session, Verdict, check_message, decode_station_message, decompress_row,
};

fn text() -> BoxedStrategy<String> {
    ({
        prop_oneof![
            3 => proptest::collection::vec(any::<char>(), 0..8).prop_map(String::from_iter),
            1 => (0usize..300).prop_map(|n| "x".repeat(n)),
        ]
    })
    .boxed()
}

fn float() -> BoxedStrategy<f32> {
    ({
        prop_oneof![
            4 => -200.0f32..200.0,
            1 => any::<f32>(),
            1 => Just(f32::NAN),
        ]
    })
    .boxed()
}

fn enum_value() -> impl Strategy<Value = i32> {
    prop_oneof![8 => 0i32..10, 1 => any::<i32>()]
}

fn small_id() -> impl Strategy<Value = u32> {
    prop_oneof![4 => 0u32..6, 1 => any::<u32>()]
}

fn freq() -> impl Strategy<Value = u64> {
    prop_oneof![4 => 0u64..7_000_000_000, 1 => any::<u64>()]
}

fn range() -> BoxedStrategy<Option<v1::FrequencyRange>> {
    ({
        proptest::option::weighted(
            0.9,
            (freq(), freq()).prop_map(|(start_hz, end_hz)| v1::FrequencyRange { start_hz, end_hz }),
        )
    })
    .boxed()
}

fn hello() -> BoxedStrategy<v1::Hello> {
    ({
        (
            proptest::option::weighted(0.9, (0u32..3, 0u32..3)),
            text(),
            proptest::option::weighted(0.9, (enum_value(), text())),
            text(),
            any::<u64>(),
        )
            .prop_map(
                |(pv, daemon_version, backend, platform, boot_id)| v1::Hello {
                    protocol_version: pv.map(|(major, minor)| v1::ProtocolVersion { major, minor }),
                    daemon_version,
                    backend: backend.map(|(kind, version)| v1::BackendInfo { kind, version }),
                    platform,
                    boot_id,
                },
            )
    })
    .boxed()
}

fn audio_params() -> BoxedStrategy<Option<v1::AudioParams>> {
    ({
        proptest::option::weighted(
            0.9,
            (
                enum_value(),
                0u32..70_000,
                prop_oneof![Just(12_000u32), any::<u32>()],
                prop_oneof![Just(20u32), 0u32..100],
            )
                .prop_map(|(codec, bitrate_bps, sample_rate_hz, frame_duration_ms)| {
                    v1::AudioParams {
                        codec,
                        bitrate_bps,
                        sample_rate_hz,
                        frame_duration_ms,
                    }
                }),
        )
    })
    .boxed()
}

fn capabilities() -> BoxedStrategy<v1::Capabilities> {
    ({
        (
            (
                range(),
                range(),
                any::<u32>(),
                prop_oneof![0u32..100, any::<u32>()],
            ),
            (
                proptest::collection::vec(enum_value(), 0..8),
                proptest::collection::vec(enum_value(), 0..3),
                proptest::collection::vec(enum_value(), 0..3),
            ),
            proptest::option::of((any::<u32>(), any::<u32>(), any::<u32>())),
            proptest::option::of((
                float(),
                float(),
                float(),
                proptest::collection::vec(text(), 0..10),
            )),
            (any::<bool>(), text()),
        )
            .prop_map(
                |((tun, hw, sr, maxch), (modes, codecs, ops), wf, fe, (fca, sdr))| {
                    v1::Capabilities {
                        tunable_range: tun,
                        hardware_range: hw,
                        sample_rate_hz: sr,
                        max_channels: maxch,
                        modes,
                        codecs,
                        waterfall: wf.map(|(max_bins, min_row_interval_ms, max_subscriptions)| {
                            v1::WaterfallLimits {
                                max_bins,
                                min_row_interval_ms,
                                max_subscriptions,
                            }
                        }),
                        frontend: fe.map(|(a, b, c, ports)| v1::FrontendControls {
                            has_gain: true,
                            min_gain_db: a,
                            max_gain_db: b,
                            has_hardware_agc: false,
                            has_attenuator: true,
                            max_attenuation_db: c,
                            antenna_ports: ports,
                            can_retune: true,
                        }),
                        allowed_admin_ops: ops,
                        frontend_control_allowed: fca,
                        sdr_model: sdr,
                    }
                },
            )
    })
    .boxed()
}

fn payload() -> BoxedStrategy<Payload> {
    ({
        prop_oneof![
            capabilities().prop_map(Payload::Capabilities),
            (
                small_id(),
                freq(),
                enum_value(),
                any::<i32>(),
                any::<i32>(),
                audio_params()
            )
                .prop_map(
                    |(channel_id, frequency_hz, mode, low_cut_hz, high_cut_hz, audio)| {
                        Payload::ChannelOpened(v1::ChannelOpened {
                            channel_id,
                            frequency_hz,
                            mode,
                            low_cut_hz,
                            high_cut_hz,
                            audio,
                        })
                    }
                ),
            (small_id(), freq(), enum_value(), any::<i32>(), any::<i32>()).prop_map(
                |(channel_id, frequency_hz, mode, low_cut_hz, high_cut_hz)| {
                    Payload::ChannelUpdated(v1::ChannelUpdated {
                        channel_id,
                        frequency_hz,
                        mode,
                        low_cut_hz,
                        high_cut_hz,
                    })
                }
            ),
            (float(), float(), text(), freq(), range()).prop_map(|(g, a, port, c, tr)| {
                Payload::FrontendState(v1::FrontendState {
                    gain_db: g,
                    hardware_agc: false,
                    attenuation_db: a,
                    antenna_port: port,
                    center_frequency_hz: c,
                    tunable_range: tr,
                })
            }),
            (
                small_id(),
                range(),
                prop_oneof![Just(256u32), any::<u32>()],
                prop_oneof![Just(100u32), any::<u32>()],
                (float(), float()),
                enum_value()
            )
                .prop_map(
                    |(subscription_id, range, bins, row_interval_ms, (o, s), compression)| {
                        Payload::WaterfallSubscribed(v1::WaterfallSubscribed {
                            subscription_id,
                            range,
                            bins,
                            row_interval_ms,
                            quantization: Some(v1::Quantization {
                                offset_db: o,
                                step_db: s,
                            }),
                            compression,
                        })
                    }
                ),
            (0u64..20, any::<i64>()).prop_map(|(nonce, t)| Payload::Pong(v1::Pong {
                nonce,
                station_time_unix_ms: t
            })),
        ]
    })
    .boxed()
}

fn body() -> BoxedStrategy<Option<Body>> {
    ({
    let b = prop_oneof![
        1 => hello().prop_map(Body::Hello),
        4 => (0u64..24, proptest::option::weighted(0.95, (enum_value(), text())), proptest::option::of(payload()))
            .prop_map(|(request_id, result, payload)| Body::Response(v1::Response {
                request_id,
                result: result.map(|(code, message)| v1::Result { code, message }),
                payload,
            })),
        3 => (small_id(), any::<u32>(), prop_oneof![0usize..1300, Just(1275usize)]).prop_map(|(channel_id, seq, n)| {
            Body::Audio(v1::AudioFrame { channel_id, seq, timestamp: seq, payload: vec![1; n] })
        }),
        3 => (small_id(), prop_oneof![
                Just(zstd_row(256)),
                proptest::collection::vec(any::<u8>(), 0..300),
            ]).prop_map(|(subscription_id, payload)| {
            Body::Waterfall(v1::WaterfallRow { subscription_id, seq: 0, timestamp_us: 0, payload })
        }),
        2 => (small_id(), float(), float(), proptest::option::of(any::<bool>())).prop_map(|(channel_id, snr_db, power_dbfs, squelch_open)| {
            Body::Signal(v1::ChannelSignal { channel_id, snr_db, power_dbfs, squelch_open })
        }),
        1 => (float(), proptest::option::of(float()), enum_value(), text(), enum_value(), float(), proptest::option::of(float())).prop_map(
            |(load, temp, bstate, bver, fstate, gain, pwr)| Body::Status(v1::StationStatus {
                uptime_s: 1,
                host: Some(v1::HostHealth { load_avg_1m: load, cpu_temp_c: temp, ..Default::default() }),
                backend: Some(v1::BackendHealth { state: bstate, version: bver }),
                frontend: Some(v1::FrontendHealth { state: fstate, gain_db: gain, input_power_dbfs: pwr, ..Default::default() }),
                ..Default::default()
            })
        ),
        2 => (enum_value(), proptest::option::of(small_id()), proptest::option::of(small_id()), text()).prop_map(
            |(kind, channel_id, subscription_id, detail)| Body::Event(v1::Event { kind, channel_id, subscription_id, detail })
        ),
    ];
    proptest::option::weighted(0.97, b)
    }).boxed()
}

fn station_message() -> impl Strategy<Value = v1::StationMessage> {
    body().prop_map(|body| v1::StationMessage { body })
}

/// An established session with channels 1–3, subscriptions 1–2 (256 bins,
/// zstd) and one outstanding request of every kind (ids 10–18).
fn busy_session(t0: Instant) -> Session {
    let mut s = established(t0);
    let mut id = 0;
    let mut send = |s: &mut Session, c: Command| {
        id += 1;
        s.on_request_sent(&request(id, c), t0).expect("request");
        id
    };
    for ch in 1..=3 {
        let rid = send(&mut s, open_channel(ch, 20));
        assert_eq!(
            s.check(&response(rid, OK, Some(channel_opened(ch, 20))), t0),
            Verdict::Accept
        );
    }
    for sub in 1..=2 {
        let rid = send(&mut s, subscribe(sub, 256, 100));
        assert_eq!(
            s.check(
                &response(rid, OK, Some(waterfall_subscribed(sub, 256, 100))),
                t0
            ),
            Verdict::Accept
        );
    }
    let _ = send(&mut s, Command::GetCapabilities(v1::GetCapabilities {}));
    let _ = send(&mut s, open_channel(4, 20));
    let _ = send(
        &mut s,
        Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 1,
            ..Default::default()
        }),
    );
    let _ = send(&mut s, close_channel(3));
    let _ = send(&mut s, Command::SetFrontend(v1::SetFrontend::default()));
    let _ = send(&mut s, subscribe(3, 256, 100));
    let _ = send(&mut s, unsubscribe(2));
    let _ = send(
        &mut s,
        Command::Ping(v1::Ping {
            nonce: 5,
            hub_time_unix_ms: 0,
        }),
    );
    let _ = send(&mut s, Command::AdminOp(v1::AdminOp { kind: 1 }));
    s
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn roundtrip_encode_decode(m in station_message()) {
        let bytes = m.encode_to_vec();
        let decoded = v1::StationMessage::decode(bytes.as_slice()).expect("decodes");
        // Compare re-encoded bytes: NaN floats make `PartialEq` unusable.
        prop_assert_eq!(decoded.encode_to_vec(), bytes);
    }

    #[test]
    fn session_check_never_panics(
        msgs in proptest::collection::vec((station_message(), 0u64..3000), 1..24),
        fresh in any::<bool>(),
    ) {
        let t0 = Instant::now();
        let mut s = if fresh { Session::new(t0) } else { busy_session(t0) };
        let mut now = t0;
        for (m, step_ms) in &msgs {
            now += Duration::from_millis(*step_ms);
            let _ = s.check(m, now);
            let _ = s.expire_requests(now);
            let _ = s.next_deadline();
            let _ = s.last_waterfall_row();
        }
    }

    #[test]
    fn stateless_rejection_is_a_session_violation(m in station_message()) {
        let t0 = Instant::now();
        let mut s = busy_session(t0);
        let is_hello = matches!(m.body, Some(Body::Hello(_)));
        if check_message(&m, 0).is_err() && !is_hello {
            prop_assert!(matches!(s.check(&m, t0), Verdict::Violation(_)));
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let t0 = Instant::now();
        if let Ok(m) = decode_station_message(&bytes, usize::MAX) {
            let _ = check_message(&m, 0);
            let _ = busy_session(t0).check(&m, t0);
            let _ = Session::new(t0).check(&m, t0);
        }
    }

    #[test]
    fn decompression_is_exact_or_rejected(
        payload in proptest::collection::vec(any::<u8>(), 0..400),
        compression in -1i32..4,
        bins in 0u32..20_000,
    ) {
        if let Ok(row) = decompress_row(&payload, compression, bins) {
            prop_assert_eq!(row.len() as u64, u64::from(bins));
        }
    }

    #[test]
    fn backoff_delays_stay_in_bounds(
        events in proptest::collection::vec((proptest::option::of((0i32..12, 0u32..100_000, any::<bool>())), any::<u64>(), 0u64..600), 1..40),
    ) {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut now = t0;
        for (goodbye, r, connected_for) in events {
            b.on_connected(now);
            now += Duration::from_secs(connected_for);
            let g = goodbye.map(|(reason, retry_after_ms, refresh_assignment)| v1::Goodbye {
                reason,
                message: String::new(),
                retry_after_ms,
                refresh_assignment,
            });
            let cap = if connected_for >= 300 { Duration::from_secs(1) } else { b.next_cap() };
            let mut rng = move || r;
            match b.on_disconnected(g.as_ref(), now, &mut rng) {
                Decision::Stop => prop_assert!(b.is_revoked()),
                Decision::Reconnect { delay, .. } => {
                    let retry = Duration::from_millis(u64::from(g.as_ref().map_or(0, |g| g.retry_after_ms)));
                    let reason = g.as_ref().map(|g| g.reason);
                    match reason {
                        Some(5) => prop_assert_eq!(delay, retry.max(Duration::from_secs(300))),
                        Some(4) => prop_assert_eq!(delay, retry.max(Duration::from_secs(21_600))),
                        _ => {
                            prop_assert!(delay >= retry);
                            prop_assert!(delay <= retry + cap);
                            prop_assert!(cap <= Duration::from_secs(60));
                        }
                    }
                }
            }
        }
    }
}
