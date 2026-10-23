// SPDX-License-Identifier: MIT
//! One test (or more) per row of station-protocol §5, each with an accept
//! path and a violation or drop path; plus §2.1 timeouts and §4 request
//! bookkeeping.

// Test tables are long by nature.
#![allow(clippy::too_many_lines, clippy::similar_names, clippy::type_complexity)]

mod common;

use std::time::Instant;

use common::*;
use station_proto::limits;
use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{
    DropReason, MessageKind, RequestError, Session, Verdict, ViolationKind, check_message,
    decode_station_message, sanitize_capabilities,
};

fn violation(v: Verdict) -> ViolationKind {
    match v {
        Verdict::Violation(k) => k,
        other => panic!("expected a violation, got {other:?}"),
    }
}

// ------------------------------------------------------------------ Size

#[test]
fn size_limit() {
    use prost::Message as _;
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    // An audio frame is small: accepted.
    assert_eq!(s.check(&audio(1, 100), t0), Verdict::Accept);
    // 70 000-byte payload: the size rule fires before the payload rule.
    let big = audio(1, 70_000);
    assert!(matches!(
        violation(s.check(&big, t0)),
        ViolationKind::MessageTooLarge { max: 65_536, .. }
    ));
    // Decoder path: size first, then protobuf.
    let bytes = big.encode_to_vec();
    assert!(matches!(
        decode_station_message(&bytes, usize::MAX),
        Err(ViolationKind::MessageTooLarge { max: 65_536, .. })
    ));
    let ok = hello().encode_to_vec();
    assert_eq!(
        decode_station_message(&ok, limits::MAX_MESSAGE_BYTES),
        Ok(hello())
    );
    assert!(matches!(
        decode_station_message(&ok, 4),
        Err(ViolationKind::MessageTooLarge { max: 4, .. })
    ));
    assert_eq!(
        decode_station_message(&[0xff, 0xff, 0xff], 100),
        Err(ViolationKind::Decode)
    );
}

#[test]
fn welcome_lowers_the_size_limit() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    s.on_welcome_sent(
        &v1::Welcome {
            max_message_bytes: 200,
            ..Default::default()
        },
        t0,
    );
    assert_eq!(s.check(&audio(1, 100), t0), Verdict::Accept);
    assert!(matches!(
        violation(s.check(&audio(1, 300), t0)),
        ViolationKind::MessageTooLarge { max: 200, .. }
    ));
    // A larger value than the protocol cap is capped.
    s.on_welcome_sent(
        &v1::Welcome {
            max_message_bytes: u32::MAX,
            ..Default::default()
        },
        t0,
    );
    assert!(matches!(
        violation(s.check(&audio(1, 70_000), t0)),
        ViolationKind::MessageTooLarge { max: 65_536, .. }
    ));
}

// ------------------------------------------------------------------ First message

#[test]
fn first_message_must_be_hello() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    assert_eq!(
        violation(s.check(&status(), t0)),
        ViolationKind::FirstMessageNotHello {
            got: MessageKind::Status
        }
    );
    let mut s = Session::new(t0);
    assert_eq!(s.negotiated_version(), None);
    assert_eq!(s.check(&hello(), t0), Verdict::Accept);
    assert_eq!(
        s.negotiated_version(),
        Some(v1::ProtocolVersion { major: 2, minor: 0 })
    );
}

#[test]
fn hello_never_again() {
    let t0 = Instant::now();
    let mut s = established(t0);
    assert_eq!(
        violation(s.check(&hello(), t0)),
        ViolationKind::DuplicateHello
    );
}

#[test]
fn empty_message_is_a_violation() {
    let t0 = Instant::now();
    let empty = v1::StationMessage { body: None };
    assert_eq!(
        violation(Session::new(t0).check(&empty, t0)),
        ViolationKind::EmptyMessage
    );
    assert_eq!(
        violation(established(t0).check(&empty, t0)),
        ViolationKind::EmptyMessage
    );
}

#[test]
fn malformed_hello() {
    let t0 = Instant::now();
    let mut h = hello_with(2, 0);
    h.protocol_version = None;
    assert_eq!(
        violation(Session::new(t0).check(&msg(Body::Hello(h)), t0)),
        ViolationKind::MissingField {
            field: "hello.protocol_version"
        }
    );
    let mut h = hello_with(2, 0);
    h.backend = None;
    assert!(is_violation(
        &Session::new(t0).check(&msg(Body::Hello(h)), t0)
    ));
    let mut h = hello_with(2, 0);
    h.backend = Some(v1::BackendInfo::default());
    assert_eq!(
        violation(Session::new(t0).check(&msg(Body::Hello(h)), t0)),
        ViolationKind::UnspecifiedEnum {
            field: "hello.backend.kind"
        }
    );
    // Hello precedes negotiation: a backend kind from a newer minor is fine.
    let mut h = hello_with(2, 3);
    h.backend = Some(v1::BackendInfo {
        kind: 17,
        version: String::new(),
    });
    assert_eq!(
        Session::new(t0).check(&msg(Body::Hello(h)), t0),
        Verdict::Accept
    );
}

// ------------------------------------------------------------------ Strings

type Build = fn(String) -> v1::StationMessage;

fn hardware_with(f: impl FnOnce(&mut v1::Sdr)) -> v1::StationMessage {
    let mut h = hardware();
    if let Some(sdr) = h.sdrs.first_mut() {
        f(sdr);
    }
    response(1, OK, Some(Payload::Hardware(h)))
}

fn status_with(f: impl FnOnce(&mut v1::ReceiverHealth)) -> v1::StationMessage {
    let mut m = status();
    if let Some(Body::Status(x)) = m.body.as_mut() {
        if let Some(rx) = x.receivers.first_mut() {
            f(rx);
        }
    }
    m
}

fn string_cases() -> [(Build, &'static str, usize); 13] {
    [
        (
            |s| {
                let mut h = hello_with(2, 0);
                h.daemon_version = s;
                msg(Body::Hello(h))
            },
            "hello.daemon_version",
            64,
        ),
        (
            |s| {
                let mut h = hello_with(2, 0);
                h.platform = s;
                msg(Body::Hello(h))
            },
            "hello.platform",
            64,
        ),
        (
            |s| {
                let mut h = hello_with(2, 0);
                h.backend = Some(v1::BackendInfo {
                    kind: 1,
                    version: s,
                });
                msg(Body::Hello(h))
            },
            "hello.backend.version",
            64,
        ),
        (
            |s| hardware_with(|sdr| sdr.manufacturer = s),
            "hardware.sdrs.manufacturer",
            128,
        ),
        (
            |s| hardware_with(|sdr| sdr.product = s),
            "hardware.sdrs.product",
            128,
        ),
        (
            |s| hardware_with(|sdr| sdr.serial = s),
            "hardware.sdrs.serial",
            128,
        ),
        (
            |s| hardware_with(|sdr| sdr.port = s),
            "hardware.sdrs.port",
            32,
        ),
        (
            |s| {
                let Payload::ReceiversConfigured(mut r) = receivers_configured(&[RX]) else {
                    unreachable!()
                };
                r.receivers[0].detail = s;
                response(1, OK, Some(Payload::ReceiversConfigured(r)))
            },
            "receivers_configured.receivers.detail",
            256,
        ),
        (
            |s| status_with(|rx| rx.detail = s),
            "status.receivers.detail",
            256,
        ),
        (
            |s| {
                msg(Body::Response(v1::Response {
                    request_id: 1,
                    result: Some(v1::Result {
                        code: NOT_FOUND,
                        message: s,
                    }),
                    payload: None,
                }))
            },
            "response.result.message",
            256,
        ),
        (
            |s| {
                let mut caps = capabilities();
                caps.receivers[0].sdr_model = s;
                response(1, OK, Some(Payload::Capabilities(caps)))
            },
            "capabilities.receivers.sdr_model",
            64,
        ),
        (
            |s| {
                msg(Body::Event(v1::Event {
                    kind: v1::event::Kind::DeviceLost as i32,
                    receiver_id: Some(RX),
                    detail: s,
                    ..Default::default()
                }))
            },
            "event.detail",
            256,
        ),
        (
            |s| {
                status_with(|rx| {
                    rx.backend = Some(v1::BackendHealth {
                        state: 1,
                        version: s,
                    });
                })
            },
            "status.receivers.backend.version",
            64,
        ),
    ]
}

#[test]
fn string_caps() {
    for (build, field, max) in string_cases() {
        assert_eq!(
            check_message(&build("é".repeat(max / 2)), 0),
            Ok(()),
            "{field} at cap"
        );
        assert_eq!(
            check_message(&build("x".repeat(max + 1)), 0),
            Err(ViolationKind::StringTooLong {
                field,
                len: max + 1,
                max
            }),
        );
        // Bytes, not characters: max/2 + 1 two-byte characters exceed the cap.
        assert!(
            check_message(&build("é".repeat(max / 2 + 1)), 0).is_err(),
            "{field}"
        );
    }
}

#[test]
fn antenna_labels() {
    let mut caps = capabilities();
    if let Some(f) = caps.receivers[0].frontend.as_mut() {
        f.antenna_ports = vec!["x".repeat(16); 8];
    }
    assert!(sanitize_capabilities(&caps, 0).is_ok());
    if let Some(f) = caps.receivers[0].frontend.as_mut() {
        f.antenna_ports.push("I".into());
    }
    assert_eq!(
        sanitize_capabilities(&caps, 0).map(drop),
        Err(ViolationKind::TooManyEntries {
            field: "capabilities.receivers.frontend.antenna_ports",
            len: 9,
            max: 8
        })
    );
    // A profile's front-end settings carry the same caps.
    let mut h = hardware();
    if let Some(f) = h.profiles[0].frontend.as_mut() {
        f.antenna_ports = vec!["x".repeat(16); 9];
    }
    assert_eq!(
        check_message(&response(1, OK, Some(Payload::Hardware(h))), 0),
        Err(ViolationKind::TooManyEntries {
            field: "hardware.profiles.frontend.antenna_ports",
            len: 9,
            max: 8
        })
    );
    let mut caps = capabilities();
    if let Some(f) = caps.receivers[0].frontend.as_mut() {
        f.antenna_ports = vec!["x".repeat(17)];
    }
    assert!(matches!(
        sanitize_capabilities(&caps, 0),
        Err(ViolationKind::StringTooLong {
            len: 17,
            max: 16,
            ..
        })
    ));
    // FrontendState.antenna_port shares the label cap.
    let Payload::FrontendState(mut fs) = frontend_state() else {
        unreachable!()
    };
    fs.antenna_port = "x".repeat(17);
    assert!(is_err(&response(1, OK, Some(Payload::FrontendState(fs)))));
}

fn is_err(m: &v1::StationMessage) -> bool {
    check_message(m, 0).is_err()
}

#[test]
fn invalid_utf8_fails_to_decode() {
    // StationMessage.hello (field 1) { daemon_version (field 2) = [0xff] }
    let bytes = [0x0a, 0x03, 0x12, 0x01, 0xff];
    assert_eq!(
        decode_station_message(&bytes, 100),
        Err(ViolationKind::Decode)
    );
    // Same structure with valid UTF-8 decodes.
    let bytes = [0x0a, 0x03, 0x12, 0x01, b'a'];
    assert!(decode_station_message(&bytes, 100).is_ok());
}

// ------------------------------------------------------------------ Enums

#[test]
fn enums_known_and_specified() {
    // Unknown result code.
    assert!(matches!(
        check_message(&response(1, 99, None), 0),
        Err(ViolationKind::UnknownEnumValue { value: 99, .. })
    ));
    // UNSPECIFIED result code.
    assert!(matches!(
        check_message(&response(1, 0, None), 0),
        Err(ViolationKind::UnspecifiedEnum { .. })
    ));
    // Missing result.
    let r = msg(Body::Response(v1::Response {
        request_id: 1,
        result: None,
        payload: None,
    }));
    assert!(matches!(
        check_message(&r, 0),
        Err(ViolationKind::MissingField { .. })
    ));
    // Event kinds.
    let mut e = event(v1::event::Kind::DeviceLost, None, None);
    assert_eq!(check_message(&e, 0), Ok(()));
    if let Some(Body::Event(ev)) = e.body.as_mut() {
        ev.kind = 42;
    }
    assert!(matches!(
        check_message(&e, 0),
        Err(ViolationKind::UnknownEnumValue {
            field: "event.kind",
            value: 42,
            minor: 0
        })
    ));
    if let Some(Body::Event(ev)) = e.body.as_mut() {
        ev.kind = 0;
    }
    assert!(matches!(
        check_message(&e, 0),
        Err(ViolationKind::UnspecifiedEnum { .. })
    ));
    // Health states in StationStatus (§5.1: present means specified).
    assert!(is_err(&status_with(|rx| {
        if let Some(f) = rx.frontend.as_mut() {
            f.state = 9;
        }
    })));
    assert!(is_err(&status_with(|rx| {
        if let Some(b) = rx.backend.as_mut() {
            b.state = 0;
        }
    })));
    assert_eq!(
        check_message(&status_with(|rx| rx.state = 0), 0),
        Err(ViolationKind::UnspecifiedEnum {
            field: "status.receivers.state"
        })
    );
    assert!(matches!(
        check_message(&status_with(|rx| rx.state = 5), 0),
        Err(ViolationKind::UnknownEnumValue {
            field: "status.receivers.state",
            value: 5,
            ..
        })
    ));
    // Absent health sub-messages are fine.
    assert!(!is_err(&status_with(|rx| {
        rx.backend = None;
        rx.frontend = None;
    })));
    // Receiver states in ReceiversConfigured.
    for bad in [0, 5] {
        let Payload::ReceiversConfigured(mut r) = receivers_configured(&[RX]) else {
            unreachable!()
        };
        r.receivers[0].state = bad;
        assert!(is_err(&response(
            1,
            OK,
            Some(Payload::ReceiversConfigured(r))
        )));
    }
    // Models and profiles in Hardware.
    for bad in [0, 4, -1] {
        let mut h = hardware();
        h.sdrs[0].models[0].model = bad;
        assert!(is_err(&response(1, OK, Some(Payload::Hardware(h)))));
        let mut h = hardware();
        h.sdrs[0].models[0].profiles.push(bad);
        assert!(is_err(&response(1, OK, Some(Payload::Hardware(h)))));
        let mut h = hardware();
        h.profiles[0].profile = bad;
        assert!(is_err(&response(1, OK, Some(Payload::Hardware(h)))));
    }
    // Enum lists in Capabilities.
    for bad in [0, 7, -1] {
        let mut caps = capabilities();
        caps.modes.push(bad);
        assert!(sanitize_capabilities(&caps, 0).is_err(), "mode {bad}");
        let mut caps = capabilities();
        caps.codecs.push(bad);
        assert!(sanitize_capabilities(&caps, 0).is_err(), "codec {bad}");
    }
    // Enums inside response payloads.
    let Payload::ChannelOpened(mut co) = channel_opened(1, 20) else {
        unreachable!()
    };
    co.mode = 0;
    assert!(is_err(&response(1, OK, Some(Payload::ChannelOpened(co)))));
    co.mode = 1;
    if let Some(a) = co.audio.as_mut() {
        a.codec = 5;
    }
    assert!(is_err(&response(1, OK, Some(Payload::ChannelOpened(co)))));
    let Payload::ChannelUpdated(mut cu) = channel_updated(1) else {
        unreachable!()
    };
    cu.mode = 100;
    assert!(is_err(&response(1, OK, Some(Payload::ChannelUpdated(cu)))));
    let Payload::WaterfallSubscribed(mut ws) = waterfall_subscribed(1, 1024, 100) else {
        unreachable!()
    };
    ws.compression = 0;
    assert!(is_err(&response(
        1,
        OK,
        Some(Payload::WaterfallSubscribed(ws))
    )));
}

// ------------------------------------------------------------------ Response payload sanity

#[test]
fn response_payload_sanity() {
    let good = [
        channel_opened(1, 20),
        channel_updated(1),
        frontend_state(),
        waterfall_subscribed(1, 1024, 100),
        pong(1),
        Payload::Capabilities(capabilities()),
        Payload::Hardware(hardware()),
        Payload::Hardware(v1::Hardware::default()),
        receivers_configured(&[RX, RX2]),
        receivers_configured(&[]),
    ];
    for p in good {
        assert_eq!(
            check_message(&response(1, OK, Some(p.clone())), 0),
            Ok(()),
            "{p:?}"
        );
    }
    let Payload::ChannelOpened(co) = channel_opened(1, 20) else {
        unreachable!()
    };
    let bad_opened: Vec<v1::ChannelOpened> = vec![
        v1::ChannelOpened { audio: None, ..co },
        v1::ChannelOpened {
            frequency_hz: 6_000_000_001,
            ..co
        },
        v1::ChannelOpened {
            low_cut_hz: 10,
            high_cut_hz: -10,
            ..co
        },
        v1::ChannelOpened {
            audio: Some(v1::AudioParams {
                bitrate_bps: 5_999,
                ..audio_params(20)
            }),
            ..co
        },
        v1::ChannelOpened {
            audio: Some(v1::AudioParams {
                bitrate_bps: 64_001,
                ..audio_params(20)
            }),
            ..co
        },
        v1::ChannelOpened {
            audio: Some(v1::AudioParams {
                sample_rate_hz: 44_100,
                ..audio_params(20)
            }),
            ..co
        },
        v1::ChannelOpened {
            audio: Some(audio_params(25)),
            ..co
        },
    ];
    for c in bad_opened {
        assert!(
            is_err(&response(1, OK, Some(Payload::ChannelOpened(c)))),
            "{c:?}"
        );
    }
    let Payload::ChannelUpdated(cu) = channel_updated(1) else {
        unreachable!()
    };
    assert!(is_err(&response(
        1,
        OK,
        Some(Payload::ChannelUpdated(v1::ChannelUpdated {
            frequency_hz: u64::MAX,
            ..cu
        }))
    )));
    assert!(is_err(&response(
        1,
        OK,
        Some(Payload::ChannelUpdated(v1::ChannelUpdated {
            low_cut_hz: 1,
            high_cut_hz: 0,
            ..cu
        }))
    )));
    let Payload::FrontendState(fs) = frontend_state() else {
        unreachable!()
    };
    for f in [
        v1::FrontendState {
            gain_db: f32::NAN,
            ..fs.clone()
        },
        v1::FrontendState {
            attenuation_db: f32::INFINITY,
            ..fs.clone()
        },
        v1::FrontendState {
            center_frequency_hz: 7_000_000_000,
            ..fs.clone()
        },
        v1::FrontendState {
            tunable_range: Some(range(2, 1)),
            ..fs.clone()
        },
        v1::FrontendState {
            tunable_range: Some(range(1, 7_000_000_000)),
            ..fs.clone()
        },
    ] {
        assert!(
            is_err(&response(1, OK, Some(Payload::FrontendState(f.clone())))),
            "{f:?}"
        );
    }
    assert!(!is_err(&response(
        1,
        OK,
        Some(Payload::FrontendState(v1::FrontendState {
            tunable_range: None,
            ..fs
        }))
    )));
    let Payload::WaterfallSubscribed(ws) = waterfall_subscribed(1, 1024, 100) else {
        unreachable!()
    };
    for w in [
        v1::WaterfallSubscribed { range: None, ..ws },
        v1::WaterfallSubscribed {
            range: Some(range(5, 4)),
            ..ws
        },
        v1::WaterfallSubscribed { bins: 63, ..ws },
        v1::WaterfallSubscribed { bins: 16_385, ..ws },
        v1::WaterfallSubscribed {
            row_interval_ms: 0,
            ..ws
        },
        v1::WaterfallSubscribed {
            quantization: None,
            ..ws
        },
        v1::WaterfallSubscribed {
            quantization: Some(v1::Quantization {
                offset_db: f32::NAN,
                step_db: 0.5,
            }),
            ..ws
        },
        v1::WaterfallSubscribed {
            quantization: Some(v1::Quantization {
                offset_db: 0.0,
                step_db: f32::NEG_INFINITY,
            }),
            ..ws
        },
    ] {
        assert!(
            is_err(&response(1, OK, Some(Payload::WaterfallSubscribed(w)))),
            "{w:?}"
        );
    }
}

// ------------------------------------------------------------------ Response (§4 matching)

#[test]
fn unsolicited_response_is_a_violation() {
    let t0 = Instant::now();
    let mut s = established(t0);
    assert_eq!(
        violation(s.check(&response(5, OK, Some(pong(0))), t0)),
        ViolationKind::UnsolicitedResponse { request_id: 5 }
    );
    // Answering the same request twice.
    let mut s = established(t0);
    s.on_request_sent(
        &request(
            1,
            Command::Ping(v1::Ping {
                nonce: 9,
                hub_time_unix_ms: 0,
            }),
        ),
        t0,
    )
    .expect("request");
    assert_eq!(
        s.check(&response(1, OK, Some(pong(9))), t0),
        Verdict::Accept
    );
    assert_eq!(
        violation(s.check(&response(1, OK, Some(pong(9))), t0)),
        ViolationKind::UnsolicitedResponse { request_id: 1 }
    );
}

#[test]
fn responses_must_echo_the_request() {
    let t0 = Instant::now();
    let cases: Vec<(Command, Payload, &str)> = vec![
        (
            open_channel(1, 20),
            channel_opened(2, 20),
            "channel_opened.channel_id",
        ),
        (
            Command::UpdateChannel(v1::UpdateChannel {
                channel_id: 1,
                ..Default::default()
            }),
            channel_updated(2),
            "channel_updated.channel_id",
        ),
        (
            open_channel_on(RX2, 1, 20),
            channel_opened(1, 20),
            "channel_opened.receiver_id",
        ),
        (
            Command::SetFrontend(v1::SetFrontend {
                receiver_id: RX2,
                ..Default::default()
            }),
            frontend_state(),
            "frontend_state.receiver_id",
        ),
        (
            subscribe_on(RX2, 1, 1024, 100),
            waterfall_subscribed(1, 1024, 100),
            "waterfall_subscribed.receiver_id",
        ),
        (
            configure(&[RX, RX2]),
            receivers_configured(&[RX]),
            "receivers_configured.receivers",
        ),
        (
            configure(&[RX]),
            receivers_configured(&[RX, RX2]),
            "receivers_configured.receivers.receiver_id",
        ),
        (
            configure(&[RX]),
            receivers_configured(&[RX, RX]),
            "receivers_configured.receivers.receiver_id",
        ),
        (
            subscribe(1, 1024, 100),
            waterfall_subscribed(2, 1024, 100),
            "waterfall_subscribed.subscription_id",
        ),
        (
            subscribe(1, 1024, 100),
            waterfall_subscribed(1, 2048, 100),
            "waterfall_subscribed.bins",
        ),
        (
            subscribe(1, 1024, 100),
            waterfall_subscribed(1, 1024, 50),
            "waterfall_subscribed.row_interval_ms",
        ),
        (
            Command::Ping(v1::Ping {
                nonce: 1,
                hub_time_unix_ms: 0,
            }),
            pong(2),
            "pong.nonce",
        ),
    ];
    for (command, payload, field) in cases {
        let mut s = established(t0);
        s.on_request_sent(&request(1, command), t0)
            .expect("request");
        match violation(s.check(&response(1, OK, Some(payload)), t0)) {
            ViolationKind::ResponseFieldMismatch {
                field: f,
                request_id: 1,
                ..
            } => assert_eq!(f, field),
            other => panic!("{field}: {other:?}"),
        }
    }
    // Fewer bins and a slower row rate than asked are fine.
    let mut s = established(t0);
    s.on_request_sent(&request(1, subscribe(1, 1024, 100)), t0)
        .expect("request");
    assert_eq!(
        s.check(
            &response(1, OK, Some(waterfall_subscribed(1, 512, 200))),
            t0
        ),
        Verdict::Accept
    );
}

// ------------------------------------------------------------------ AudioFrame

#[test]
fn audio_payload_size() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    assert_eq!(s.check(&audio(1, 1), t0), Verdict::Accept);
    assert_eq!(s.check(&audio(1, 1275), t0), Verdict::Accept);
    assert_eq!(
        violation(s.check(&audio(1, 0), t0)),
        ViolationKind::AudioPayloadSize { len: 0 }
    );
    assert_eq!(
        violation(s.check(&audio(1, 1276), t0)),
        ViolationKind::AudioPayloadSize { len: 1276 }
    );
}

#[test]
fn audio_only_for_open_channels() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    assert_eq!(s.check(&audio(1, 100), t0), Verdict::Accept);
    assert_eq!(
        violation(s.check(&audio(2, 100), t0)),
        ViolationKind::UnknownChannel {
            kind: MessageKind::Audio,
            channel_id: 2
        }
    );
}

#[test]
fn audio_at_nominal_rate_is_accepted_for_a_minute() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    // 20 ms frames: 50/s nominal, 75/s allowed.
    for i in 0..3000 {
        assert_eq!(
            s.check(&audio(1, 80), t0 + ms(20 * i)),
            Verdict::Accept,
            "frame {i}"
        );
    }
}

#[test]
fn audio_over_rate_drops_then_violation_after_ten_seconds() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    // 100 frames/s (2x nominal) against a 75/s allowance.
    let mut first_drop = None;
    let mut violation_at = None;
    for i in 0..3000u64 {
        let now = t0 + ms(10 * i);
        match s.check(&audio(1, 80), now) {
            Verdict::Accept => {}
            Verdict::Drop(DropReason::RateLimited {
                kind: MessageKind::Audio,
            }) => {
                first_drop.get_or_insert(now);
            }
            Verdict::Violation(ViolationKind::AudioRateSustained { channel_id: 1 }) => {
                violation_at = Some(now);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let first = first_drop.expect("drops");
    let at = violation_at.expect("violation");
    let sustained = at - first;
    assert!(
        sustained >= secs(10) && sustained < secs(11),
        "{sustained:?}"
    );
}

#[test]
fn audio_short_bursts_only_drop() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    let mut now = t0;
    // Repeatedly: 3 s at 2x the allowance, then 3 s of silence.
    for _ in 0..10 {
        for _ in 0..450 {
            let v = s.check(&audio(1, 80), now);
            assert!(!is_violation(&v), "{v:?}");
            now += ms(6);
        }
        now += secs(3);
    }
}

#[test]
fn confirmed_frame_duration_sets_the_rate() {
    let t0 = Instant::now();
    let mut s = established(t0);
    // Requested 60 ms (25/s allowed), confirmed 10 ms (150/s allowed).
    s.on_request_sent(&request(1, open_channel(1, 60)), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(1, OK, Some(channel_opened(1, 10))), t0),
        Verdict::Accept
    );
    for i in 0..1000 {
        assert_eq!(
            s.check(&audio(1, 40), t0 + ms(10 * i)),
            Verdict::Accept,
            "frame {i}"
        );
    }
}

#[test]
fn audio_may_race_the_open_response() {
    let t0 = Instant::now();
    let mut s = established(t0);
    s.on_request_sent(&request(1, open_channel(1, 20)), t0)
        .expect("request");
    assert_eq!(s.check(&audio(1, 80), t0), Verdict::Accept);
    // A failed open removes the channel.
    assert_eq!(
        s.check(
            &response(1, v1::ResultCode::ResourceExhausted as i32, None),
            t0
        ),
        Verdict::Accept
    );
    assert!(!s.is_channel_open(1, t0));
    assert!(is_violation(&s.check(&audio(1, 80), t0)));
}

// ------------------------------------------------------------------ WaterfallRow

#[test]
fn waterfall_rows_decompress_to_exactly_bins() {
    let t0 = Instant::now();
    let mut s = with_subscription(t0, 3, 1024, 100);
    assert_eq!(s.check(&waterfall(3, zstd_row(1024)), t0), Verdict::Accept);
    assert_eq!(s.last_waterfall_row(), row_bytes(1024).as_slice());
    assert_eq!(
        violation(s.check(&waterfall(3, zstd_row(1023)), t0 + ms(100))),
        ViolationKind::WaterfallLength {
            expected: 1024,
            actual: 1023
        }
    );
    let mut s = with_subscription(t0, 3, 1024, 100);
    assert_eq!(
        violation(s.check(&waterfall(3, zstd_row(4096)), t0)),
        ViolationKind::WaterfallLength {
            expected: 1024,
            actual: 1025
        }
    );
    let mut s = with_subscription(t0, 3, 1024, 100);
    assert_eq!(
        violation(s.check(&waterfall(3, b"garbage".to_vec()), t0)),
        ViolationKind::WaterfallDecompression
    );
}

#[test]
fn waterfall_compressed_size_cap() {
    let t0 = Instant::now();
    let mut s = with_subscription(t0, 3, 1024, 100);
    assert_eq!(
        violation(s.check(&waterfall(3, vec![0; 16 * 1024 + 1]), t0)),
        ViolationKind::WaterfallPayloadTooLarge { len: 16_385 }
    );
}

#[test]
fn waterfall_only_for_open_subscriptions() {
    let t0 = Instant::now();
    let mut s = with_subscription(t0, 3, 1024, 100);
    assert_eq!(
        violation(s.check(&waterfall(4, zstd_row(1024)), t0)),
        ViolationKind::UnknownSubscription { subscription_id: 4 }
    );
}

#[test]
fn waterfall_rows_before_subscribed_are_dropped() {
    let t0 = Instant::now();
    let mut s = established(t0);
    s.on_request_sent(&request(1, subscribe(3, 1024, 100)), t0)
        .expect("request");
    assert_eq!(
        s.check(&waterfall(3, zstd_row(1024)), t0),
        Verdict::Drop(DropReason::AwaitingSubscription { subscription_id: 3 })
    );
    // A failed subscribe removes it.
    assert_eq!(
        s.check(&response(1, v1::ResultCode::OutOfRange as i32, None), t0),
        Verdict::Accept
    );
    assert!(!s.is_subscription_open(3, t0));
    assert!(is_violation(&s.check(&waterfall(3, zstd_row(1024)), t0)));
}

#[test]
fn waterfall_rate() {
    let t0 = Instant::now();
    // 100 ms rows: 10/s nominal, 15/s allowed.
    let mut s = with_subscription(t0, 3, 256, 100);
    let row = zstd_row(256);
    for i in 0..600 {
        assert_eq!(
            s.check(&waterfall(3, row.clone()), t0 + ms(100 * i)),
            Verdict::Accept
        );
    }
    let mut s = with_subscription(t0, 3, 256, 100);
    let mut result = None;
    for i in 0..200u64 {
        let v = s.check(&waterfall(3, row.clone()), t0 + ms(40 * i));
        if v != Verdict::Accept {
            result = Some((i, v));
            break;
        }
    }
    let (i, v) = result.expect("rate violation");
    assert_eq!(
        violation(v),
        ViolationKind::WaterfallRate { subscription_id: 3 }
    );
    // 25 rows/s against 15/s with a 15-row burst: about 1.5 s.
    assert!((30..45).contains(&i), "{i}");
}

// ------------------------------------------------------------------ ChannelSignal

#[test]
fn channel_signal_rate_and_floats() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    for _ in 0..5 {
        assert_eq!(s.check(&signal(1, 10.0), t0), Verdict::Accept);
    }
    assert_eq!(
        s.check(&signal(1, 10.0), t0),
        Verdict::Drop(DropReason::RateLimited {
            kind: MessageKind::Signal
        })
    );
    assert_eq!(s.check(&signal(1, 10.0), t0 + ms(200)), Verdict::Accept);
    // Excess is never a violation, however long.
    for i in 0..1000 {
        assert!(!is_violation(&s.check(&signal(1, 1.0), t0 + ms(i))));
    }
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            violation(s.check(&signal(1, bad), t0 + secs(5))),
            ViolationKind::NonFiniteFloat {
                field: "signal.snr_db"
            }
        );
    }
    let mut m = signal(1, 1.0);
    if let Some(Body::Signal(x)) = m.body.as_mut() {
        x.power_dbfs = f32::NAN;
    }
    assert!(is_violation(&s.check(&m, t0 + secs(5))));
    assert_eq!(
        violation(s.check(&signal(9, 1.0), t0)),
        ViolationKind::UnknownChannel {
            kind: MessageKind::Signal,
            channel_id: 9
        }
    );
}

// ------------------------------------------------------------------ StationStatus

#[test]
fn status_rate_follows_the_interval() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX]);
    // Default 10 s interval: at most one per 5 s.
    assert_eq!(s.check(&status(), t0), Verdict::Accept);
    assert_eq!(
        s.check(&status(), t0 + secs(1)),
        Verdict::Drop(DropReason::RateLimited {
            kind: MessageKind::Status
        })
    );
    assert_eq!(s.check(&status(), t0 + secs(5)), Verdict::Accept);
    // Every 10 s for an hour: always accepted.
    for i in 1..360 {
        assert_eq!(s.check(&status(), t0 + secs(5 + 10 * i)), Verdict::Accept);
    }
    // Welcome with a 2 s interval: one per second.
    let mut s = configured(t0, &[RX]);
    s.on_welcome_sent(
        &v1::Welcome {
            status_interval_ms: 2000,
            ..Default::default()
        },
        t0,
    );
    assert_eq!(s.check(&status(), t0), Verdict::Accept);
    assert!(matches!(s.check(&status(), t0 + ms(500)), Verdict::Drop(_)));
    assert_eq!(s.check(&status(), t0 + ms(1000)), Verdict::Accept);
    // Interval 0 means the default.
    s.on_welcome_sent(&v1::Welcome::default(), t0 + secs(10));
    assert_eq!(s.check(&status(), t0 + secs(10)), Verdict::Accept);
    assert!(matches!(
        s.check(&status(), t0 + secs(14)),
        Verdict::Drop(_)
    ));
}

#[test]
fn status_floats_must_be_finite() {
    let t0 = Instant::now();
    let setters: [fn(&mut v1::StationStatus); 5] = [
        |x| {
            x.host
                .as_mut()
                .into_iter()
                .for_each(|h| h.load_avg_1m = f32::NAN);
        },
        |x| {
            x.host
                .as_mut()
                .into_iter()
                .for_each(|h| h.cpu_temp_c = Some(f32::INFINITY));
        },
        |x| {
            x.receivers
                .iter_mut()
                .filter_map(|r| r.frontend.as_mut())
                .for_each(|f| f.gain_db = f32::NAN);
        },
        |x| {
            x.receivers
                .iter_mut()
                .filter_map(|r| r.frontend.as_mut())
                .for_each(|f| f.input_power_dbfs = Some(f32::NAN));
        },
        |x| {
            x.receivers
                .iter_mut()
                .for_each(|r| r.backend_cpu = Some(f32::NEG_INFINITY));
        },
    ];
    for set in setters {
        let mut m = status();
        if let Some(Body::Status(x)) = m.body.as_mut() {
            set(x);
        }
        assert!(matches!(
            violation(configured(t0, &[RX]).check(&m, t0)),
            ViolationKind::NonFiniteFloat { .. }
        ));
    }
    // An empty status is fine, before a receiver set too.
    let empty = msg(Body::Status(v1::StationStatus::default()));
    assert_eq!(established(t0).check(&empty, t0), Verdict::Accept);
    assert_eq!(configured(t0, &[]).check(&empty, t0), Verdict::Accept);
}

#[test]
fn status_receivers_configured_once() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX, RX2]);
    assert_eq!(s.check(&status_of(&[RX2, RX]), t0), Verdict::Accept);
    // A receiver twice is malformed.
    assert_eq!(
        violation(configured(t0, &[RX]).check(&status_of(&[RX, RX]), t0)),
        ViolationKind::DuplicateReceiver {
            field: "status.receivers.receiver_id",
            receiver_id: RX
        }
    );
    // More than 16.
    let many: Vec<u32> = (1..=17).collect();
    assert!(matches!(
        check_message(&status_of(&many), 0),
        Err(ViolationKind::TooManyEntries {
            field: "status.receivers",
            len: 17,
            max: 16
        })
    ));
    // A receiver the hub never configured.
    assert_eq!(
        violation(configured(t0, &[RX]).check(&status_of(&[RX, 9]), t0)),
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Status,
            receiver_id: 9
        }
    );
    assert!(matches!(
        violation(configured(t0, &[RX]).check(&status_of(&[0]), t0)),
        ViolationKind::UnknownReceiver { receiver_id: 0, .. }
    ));
}

// ------------------------------------------------------------------ Event

#[test]
fn event_rate_burst_fifty_then_ten_per_second() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX]);
    let e = event(v1::event::Kind::FrontendOverload, None, None);
    for _ in 0..50 {
        assert_eq!(s.check(&e, t0), Verdict::Accept);
    }
    assert_eq!(
        s.check(&e, t0),
        Verdict::Drop(DropReason::RateLimited {
            kind: MessageKind::Event
        })
    );
    let t1 = t0 + secs(1);
    for _ in 0..10 {
        assert_eq!(s.check(&e, t1), Verdict::Accept);
    }
    assert!(matches!(s.check(&e, t1), Verdict::Drop(_)));
    // Sustained 10/s is fine.
    for i in 0..600 {
        assert_eq!(s.check(&e, t1 + ms(100 * (i + 1))), Verdict::Accept);
    }
}

#[test]
fn event_ids_must_be_open_or_just_closed() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    let closed = |id| event(v1::event::Kind::ChannelClosed, Some(id), None);
    assert_eq!(
        s.check(&closed(9), t0),
        Verdict::Drop(DropReason::UnknownEventTarget)
    );
    assert_eq!(
        s.check(
            &event(v1::event::Kind::SubscriptionClosed, None, Some(9)),
            t0
        ),
        Verdict::Drop(DropReason::UnknownEventTarget)
    );
    // Hub closes channel 1; the station's CHANNEL_CLOSED within 5 s is fine.
    s.on_request_sent(&request(2, close_channel(1)), t0)
        .expect("request");
    assert_eq!(s.check(&closed(1), t0 + secs(5)), Verdict::Accept);
    assert_eq!(
        s.check(&closed(1), t0 + ms(5001)),
        Verdict::Drop(DropReason::UnknownEventTarget)
    );
    // Missing id on a *_CLOSED event is malformed.
    assert_eq!(
        violation(s.check(&event(v1::event::Kind::ChannelClosed, None, None), t0)),
        ViolationKind::MissingField {
            field: "event.channel_id"
        }
    );
    assert_eq!(
        violation(s.check(&event(v1::event::Kind::SubscriptionClosed, None, None), t0)),
        ViolationKind::MissingField {
            field: "event.subscription_id"
        }
    );
}

#[test]
fn events_name_their_receiver() {
    use v1::event::Kind;
    let t0 = Instant::now();
    // §5.1: these kinds without receiver_id are violations.
    for kind in [
        Kind::ReceiverState,
        Kind::DeviceLost,
        Kind::DeviceRestored,
        Kind::FrontendOverload,
        Kind::FrontendChanged,
        Kind::BackendRestarted,
    ] {
        let mut m = event(kind, None, None);
        assert_eq!(check_message(&m, 0), Ok(()), "{kind:?}");
        if let Some(Body::Event(e)) = m.body.as_mut() {
            e.receiver_id = None;
        }
        assert_eq!(
            violation(configured(t0, &[RX]).check(&m, t0)),
            ViolationKind::MissingField {
                field: "event.receiver_id"
            },
            "{kind:?}"
        );
    }
    // RECEIVER_STATE needs a specified state.
    for state in [None, Some(0), Some(9)] {
        let mut m = event(Kind::ReceiverState, None, None);
        if let Some(Body::Event(e)) = m.body.as_mut() {
            e.receiver_state = state;
        }
        assert!(is_err(&m), "{state:?}");
    }
    let mut m = event(Kind::ReceiverState, None, None);
    if let Some(Body::Event(e)) = m.body.as_mut() {
        e.receiver_state = Some(v1::ReceiverState::NoDevice as i32);
        e.detail = "SDR 00000001 is not plugged in".into();
    }
    assert_eq!(configured(t0, &[RX]).check(&m, t0), Verdict::Accept);
    // HARDWARE_CHANGED and SHUTTING_DOWN name no receiver.
    for kind in [Kind::HardwareChanged, Kind::ShuttingDown] {
        assert_eq!(
            configured(t0, &[RX]).check(&event(kind, None, None), t0),
            Verdict::Accept
        );
    }
    // A receiver the hub didn't configure: dropped, never a violation.
    let mut s = configured(t0, &[RX]);
    assert_eq!(
        s.check(&receiver_event(Kind::FrontendOverload, 7), t0),
        Verdict::Drop(DropReason::UnknownEventTarget)
    );
    // Before the station accepted a set it runs the one it kept: dropped.
    let mut s = established(t0);
    assert_eq!(
        s.check(&receiver_event(Kind::DeviceLost, 7), t0),
        Verdict::Drop(DropReason::AwaitingReceivers {
            kind: MessageKind::Event
        })
    );
}

#[test]
fn station_side_close_events_close_ids() {
    let t0 = Instant::now();
    let mut s = configured(t0, &[RX, RX2]);
    s.on_request_sent(&request(2, open_channel(1, 20)), t0)
        .expect("request");
    s.on_request_sent(&request(3, subscribe(5, 256, 100)), t0)
        .expect("request");
    assert_eq!(
        s.check(
            &response(3, OK, Some(waterfall_subscribed(5, 256, 100))),
            t0
        ),
        Verdict::Accept
    );
    // A channel on the other receiver.
    s.on_request_sent(&request(4, open_channel_on(RX2, 2, 20)), t0)
        .expect("request");
    // Station closed channel 1 (e.g. SetFrontend moved the passband).
    assert_eq!(
        s.check(&event(v1::event::Kind::ChannelClosed, Some(1), None), t0),
        Verdict::Accept
    );
    assert!(!s.is_channel_open(1, t0));
    assert!(matches!(
        s.check(&audio(1, 10), t0 + secs(1)),
        Verdict::Drop(DropReason::RecentlyClosed { .. })
    ));
    assert!(is_violation(&s.check(&audio(1, 10), t0 + secs(6))));
    // Subscription closed by the station.
    let mut s2 = with_subscription(t0, 5, 256, 100);
    assert_eq!(
        s2.check(
            &event(v1::event::Kind::SubscriptionClosed, None, Some(5)),
            t0
        ),
        Verdict::Accept
    );
    assert!(!s2.is_subscription_open(5, t0));
    // A backend restart closes everything on its receiver, nothing else.
    assert!(s.is_subscription_open(5, t0));
    assert_eq!(
        s.check(&event(v1::event::Kind::BackendRestarted, None, None), t0),
        Verdict::Accept
    );
    assert!(!s.is_subscription_open(5, t0));
    assert!(is_violation(
        &s.check(&waterfall(5, zstd_row(256)), t0 + secs(6))
    ));
    assert!(s.is_channel_open(2, t0 + secs(6)));
    assert_eq!(
        s.check(
            &receiver_event(v1::event::Kind::BackendRestarted, RX2),
            t0 + secs(6)
        ),
        Verdict::Accept
    );
    assert!(!s.is_channel_open(2, t0 + secs(6)));
}

// ------------------------------------------------------------------ ID lifecycle and grace

#[test]
fn close_grace_window() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    s.on_request_sent(&request(2, close_channel(1)), t0)
        .expect("request");
    assert!(!s.is_channel_open(1, t0));
    // In-flight media within 5 s: dropped, not a violation.
    for (m, kind) in [
        (audio(1, 10), MessageKind::Audio),
        (signal(1, 1.0), MessageKind::Signal),
    ] {
        assert_eq!(
            s.check(&m, t0 + secs(5)),
            Verdict::Drop(DropReason::RecentlyClosed { kind, id: 1 })
        );
        assert!(is_violation(&s.check(&m, t0 + ms(5001))));
    }
    // Closing twice keeps the first close time.
    s.on_request_sent(&request(3, close_channel(1)), t0 + secs(4))
        .expect("request");
    assert!(is_violation(&s.check(&audio(1, 10), t0 + ms(5001))));
    // CloseChannel for an unknown id is harmless (station answers NOT_FOUND).
    s.on_request_sent(&request(4, close_channel(77)), t0)
        .expect("request");
    assert_eq!(s.check(&response(4, NOT_FOUND, None), t0), Verdict::Accept);

    let mut s = with_subscription(t0, 3, 256, 100);
    s.on_request_sent(&request(2, unsubscribe(3)), t0)
        .expect("request");
    assert_eq!(
        s.check(&waterfall(3, zstd_row(256)), t0 + secs(2)),
        Verdict::Drop(DropReason::RecentlyClosed {
            kind: MessageKind::Waterfall,
            id: 3
        })
    );
    assert!(is_violation(
        &s.check(&waterfall(3, zstd_row(256)), t0 + secs(6))
    ));
}

#[test]
fn ids_can_be_reused_after_close() {
    let t0 = Instant::now();
    let mut s = with_channel(t0, 1);
    assert_eq!(
        s.on_request_sent(&request(2, open_channel(1, 20)), t0),
        Err(RequestError::ChannelInUse(1))
    );
    s.on_request_sent(&request(3, close_channel(1)), t0)
        .expect("close");
    // Reopen inside the grace period.
    s.on_request_sent(&request(4, open_channel(1, 20)), t0 + secs(1))
        .expect("reopen");
    assert!(s.is_channel_open(1, t0 + secs(1)));
    assert_eq!(s.check(&audio(1, 10), t0 + secs(10)), Verdict::Accept);

    let mut s = with_subscription(t0, 3, 256, 100);
    assert_eq!(
        s.on_request_sent(&request(2, subscribe(3, 256, 100)), t0),
        Err(RequestError::SubscriptionInUse(3))
    );
    s.on_request_sent(&request(3, unsubscribe(3)), t0)
        .expect("unsubscribe");
    // After the grace period the old entry is garbage collected.
    s.on_request_sent(&request(4, subscribe(3, 256, 100)), t0 + secs(30))
        .expect("resubscribe");
    assert!(s.is_subscription_open(3, t0 + secs(30)));
    // Unsubscribe of an unknown id is harmless.
    s.on_request_sent(&request(5, unsubscribe(99)), t0 + secs(30))
        .expect("unsubscribe");
}

// ------------------------------------------------------------------ Capabilities

#[test]
fn capabilities_accept_and_clamp() {
    let t0 = Instant::now();
    let mut s = established(t0);
    s.on_request_sent(
        &request(1, Command::GetCapabilities(v1::GetCapabilities {})),
        t0,
    )
    .expect("request");
    assert_eq!(
        s.check(
            &response(1, OK, Some(Payload::Capabilities(capabilities()))),
            t0
        ),
        Verdict::Accept
    );

    let clean = sanitize_capabilities(&capabilities(), 0).expect("valid");
    assert!(clean.clamped.is_empty());
    assert_eq!(clean.capabilities, capabilities());

    let mut caps = capabilities();
    caps.receivers[0].hardware_range = Some(range(0, 7_250_000_000));
    caps.receivers[0].tunable_range = Some(range(5_000_000_000, 6_500_000_000));
    caps.receivers[0].max_channels = 1000;
    let mut second = receiver_capabilities(RX2, 13_000_000);
    second.max_channels = 65;
    caps.receivers.push(second);
    caps.waterfall = Some(v1::WaterfallLimits {
        max_bins: 1 << 20,
        min_row_interval_ms: 0,
        max_subscriptions: 100,
    });
    let out = sanitize_capabilities(&caps, 0).expect("clamped, not a violation");
    assert_eq!(
        out.clamped,
        [
            "capabilities.receivers.hardware_range.end_hz",
            "capabilities.receivers.tunable_range.end_hz",
            "capabilities.receivers.max_channels",
            "capabilities.waterfall.max_bins",
            "capabilities.waterfall.max_subscriptions"
        ]
    );
    let c = out.capabilities;
    assert_eq!(c.receivers[0].hardware_range, Some(range(0, 6_000_000_000)));
    assert_eq!(
        c.receivers[0].tunable_range,
        Some(range(5_000_000_000, 6_000_000_000))
    );
    assert_eq!(c.receivers[0].max_channels, 64);
    assert_eq!(c.receivers[1].max_channels, 64);
    assert_eq!(
        c.waterfall.map(|w| (w.max_bins, w.max_subscriptions)),
        Some((16_384, 16))
    );
    // Clamping is not a violation in the session either.
    let mut s = established(t0);
    s.on_request_sent(
        &request(1, Command::GetCapabilities(v1::GetCapabilities {})),
        t0,
    )
    .expect("request");
    assert_eq!(
        s.check(&response(1, OK, Some(Payload::Capabilities(caps))), t0),
        Verdict::Accept
    );
}

#[test]
fn nonsense_capabilities_are_violations() {
    let base = capabilities();
    let with_rx = |f: &dyn Fn(&mut v1::ReceiverCapabilities)| {
        let mut c = capabilities();
        f(&mut c.receivers[0]);
        c
    };
    let mut cases: Vec<(v1::Capabilities, ViolationKind)> = vec![
        (
            with_rx(&|r| r.tunable_range = Some(range(0, 1_800_000_000))),
            ViolationKind::TunableOutsideHardware { receiver_id: RX },
        ),
        (
            with_rx(&|r| {
                r.tunable_range = Some(range(1, 2));
                r.hardware_range = Some(range(10, 1_000));
            }),
            ViolationKind::TunableOutsideHardware { receiver_id: RX },
        ),
        (
            with_rx(&|r| r.hardware_range = Some(range(10, 5))),
            ViolationKind::InvalidRange {
                field: "capabilities.receivers.hardware_range",
            },
        ),
        (
            with_rx(&|r| r.tunable_range = Some(range(10, 5))),
            ViolationKind::InvalidRange {
                field: "capabilities.receivers.tunable_range",
            },
        ),
        (
            with_rx(&|r| {
                r.hardware_range = Some(range(6_000_000_001, 7_000_000_000));
                r.tunable_range = Some(range(6_000_000_001, 6_100_000_000));
            }),
            ViolationKind::InvalidRange {
                field: "capabilities.receivers.hardware_range",
            },
        ),
        (
            with_rx(&|r| r.hardware_range = None),
            ViolationKind::MissingField {
                field: "capabilities.receivers.hardware_range",
            },
        ),
        (
            with_rx(&|r| r.tunable_range = None),
            ViolationKind::MissingField {
                field: "capabilities.receivers.tunable_range",
            },
        ),
        (
            with_rx(&|r| r.receiver_id = 0),
            ViolationKind::UnknownReceiver {
                kind: MessageKind::Response,
                receiver_id: 0,
            },
        ),
    ];
    // A receiver twice.
    let mut twice = capabilities();
    twice.receivers.push(receiver_capabilities(RX, 13_000_000));
    cases.push((
        twice,
        ViolationKind::DuplicateReceiver {
            field: "capabilities.receivers.receiver_id",
            receiver_id: RX,
        },
    ));
    // More than 16.
    let mut many = capabilities();
    many.receivers = (1..=17)
        .map(|id| receiver_capabilities(id, u64::from(id) * 10_000_000))
        .collect();
    cases.push((
        many,
        ViolationKind::TooManyEntries {
            field: "capabilities.receivers",
            len: 17,
            max: 16,
        },
    ));
    for field in 0..3 {
        let mut c = base.clone();
        if let Some(f) = c.receivers[0].frontend.as_mut() {
            match field {
                0 => f.min_gain_db = f32::NAN,
                1 => f.max_gain_db = f32::INFINITY,
                _ => f.max_attenuation_db = f32::NEG_INFINITY,
            }
        }
        let err = sanitize_capabilities(&c, 0).map(drop).expect_err("nan");
        assert!(matches!(err, ViolationKind::NonFiniteFloat { .. }));
    }
    for (caps, expected) in cases {
        assert_eq!(
            sanitize_capabilities(&caps, 0).map(drop),
            Err(expected.clone())
        );
        let t0 = Instant::now();
        let mut s = established(t0);
        s.on_request_sent(
            &request(1, Command::GetCapabilities(v1::GetCapabilities {})),
            t0,
        )
        .expect("request");
        assert_eq!(
            violation(s.check(&response(1, OK, Some(Payload::Capabilities(caps))), t0)),
            expected
        );
    }
    // No frontend / no waterfall section / no receiver at all is fine.
    let mut bare = v1::Capabilities {
        waterfall: None,
        ..base
    };
    bare.receivers[0].frontend = None;
    assert!(sanitize_capabilities(&bare, 0).is_ok());
    bare.receivers.clear();
    assert!(sanitize_capabilities(&bare, 0).is_ok());
}

/// §5 `Capabilities.cw_pitch_hz`: a claim for display. In bounds it is kept,
/// absent (0) it stays absent, and outside 200 to 1200 Hz it is dropped to 0
/// and listed, never a violation and never clamped.
#[test]
fn cw_pitch_out_of_bounds_is_dropped() {
    for (claim, kept) in [
        (0, 0),
        (200, 200),
        (500, 500),
        (1200, 1200),
        (199, 0),
        (1201, 0),
        (u32::MAX, 0),
    ] {
        let caps = v1::Capabilities {
            cw_pitch_hz: claim,
            ..capabilities()
        };
        let sanitized = sanitize_capabilities(&caps, 0).expect("not a violation");
        assert_eq!(sanitized.capabilities.cw_pitch_hz, kept, "claim {claim}");
        let listed = sanitized.clamped.contains(&"capabilities.cw_pitch_hz");
        assert_eq!(listed, claim != 0 && kept == 0, "claim {claim}");
    }
}

#[test]
fn overlapping_receivers_are_not_a_violation() {
    let t0 = Instant::now();
    let mut caps = capabilities();
    // 5.9–8.3 MHz and 7.0–9.4 MHz.
    caps.receivers.push(receiver_capabilities(RX2, 7_000_000));
    assert!(sanitize_capabilities(&caps, 0).is_ok());
    let mut s = configured(t0, &[RX, RX2]);
    s.on_request_sent(
        &request(2, Command::GetCapabilities(v1::GetCapabilities {})),
        t0,
    )
    .expect("request");
    assert_eq!(
        s.check(&response(2, OK, Some(Payload::Capabilities(caps))), t0),
        Verdict::Accept
    );
    assert_eq!(s.running_receivers(), [RX, RX2]);
}

// ------------------------------------------------------------------ §2.1 timeouts

#[test]
fn response_timeouts_and_late_responses() {
    let t0 = Instant::now();
    let mut s = established(t0);
    let ping = |n| {
        Command::Ping(v1::Ping {
            nonce: n,
            hub_time_unix_ms: 0,
        })
    };
    s.on_request_sent(&request(1, ping(1)), t0)
        .expect("request");
    s.on_request_sent(
        &request(2, Command::GetCapabilities(v1::GetCapabilities {})),
        t0,
    )
    .expect("request");
    assert_eq!(s.outstanding_requests(), 2);
    assert_eq!(s.next_deadline(), t0 + secs(5));
    assert_eq!(
        s.expire_requests(t0 + ms(4999)).request_ids,
        Vec::<u64>::new()
    );
    let e = s.expire_requests(t0 + secs(5));
    assert_eq!(e.request_ids, [1]);
    assert_eq!(e.violation, None);
    // GetCapabilities gets 10 s.
    assert_eq!(s.next_deadline(), t0 + secs(10));
    let e = s.expire_requests(t0 + secs(10));
    assert_eq!(e.request_ids, [2]);
    assert_eq!(e.violation, None);
    // Late answers are dropped once, then unsolicited.
    assert_eq!(
        s.check(&response(1, OK, Some(pong(1))), t0 + secs(11)),
        Verdict::Drop(DropReason::LateResponse { request_id: 1 })
    );
    assert!(is_violation(
        &s.check(&response(1, OK, Some(pong(1))), t0 + secs(11))
    ));
    // Third consecutive timeout: violation.
    s.on_request_sent(&request(3, ping(3)), t0 + secs(20))
        .expect("request");
    let e = s.expire_requests(t0 + secs(25));
    assert_eq!(e.request_ids, [3]);
    assert_eq!(
        e.violation,
        Some(ViolationKind::ResponseTimeouts { consecutive: 3 })
    );
}

#[test]
fn an_in_time_response_resets_the_timeout_count() {
    let t0 = Instant::now();
    let mut s = established(t0);
    let ping = |n| {
        Command::Ping(v1::Ping {
            nonce: n,
            hub_time_unix_ms: 0,
        })
    };
    let mut id = 0;
    let mut now = t0;
    for round in 0..5 {
        for _ in 0..2 {
            id += 1;
            s.on_request_sent(&request(id, ping(id)), now)
                .expect("request");
            now += secs(6);
            let e = s.expire_requests(now);
            assert_eq!(e.violation, None, "round {round}");
        }
        id += 1;
        s.on_request_sent(&request(id, ping(id)), now)
            .expect("request");
        assert_eq!(
            s.check(&response(id, OK, Some(pong(id))), now),
            Verdict::Accept
        );
    }
}

#[test]
fn timed_out_memory_is_bounded() {
    let t0 = Instant::now();
    let mut s = established(t0);
    let mut now = t0;
    for id in 1..=100u64 {
        s.on_request_sent(
            &request(
                id,
                Command::AdminOp(v1::AdminOp {
                    kind: 1,
                    receiver_id: None,
                }),
            ),
            now,
        )
        .expect("request");
        now += secs(6);
        s.expire_requests(now);
    }
    // The oldest timed-out ids are forgotten: a very late answer is unsolicited.
    assert!(is_violation(&s.check(&response(1, OK, None), now)));
    assert_eq!(
        s.check(&response(100, OK, None), now),
        Verdict::Drop(DropReason::LateResponse { request_id: 100 })
    );
}

#[test]
fn hello_and_idle_timeouts() {
    let t0 = Instant::now();
    let mut s = Session::new(t0);
    assert_eq!(s.next_deadline(), t0 + limits::HELLO_TIMEOUT);
    assert!(!s.hello_timed_out(t0 + ms(9_999)));
    assert!(s.hello_timed_out(t0 + secs(10)));
    assert_eq!(s.check(&hello(), t0 + secs(3)), Verdict::Accept);
    assert!(!s.hello_timed_out(t0 + secs(100)));
    assert_eq!(s.next_deadline(), t0 + secs(3) + limits::IDLE_TIMEOUT);
    assert!(!s.idle_timed_out(t0 + secs(47)));
    assert!(s.idle_timed_out(t0 + secs(48)));
    // Any message, even a dropped one, counts as life.
    let _ = s.check(&status(), t0 + secs(40));
    let _ = s.check(&status(), t0 + secs(41));
    assert!(!s.idle_timed_out(t0 + secs(85)));
    assert!(s.idle_timed_out(t0 + secs(86)));
}

// ------------------------------------------------------------------ §4 request bookkeeping

#[test]
fn request_bookkeeping_errors() {
    let t0 = Instant::now();
    let ping = || Command::Ping(v1::Ping::default());
    let mut s = Session::new(t0);
    assert_eq!(
        s.on_request_sent(&request(1, ping()), t0),
        Err(RequestError::NotEstablished)
    );
    let mut s = established(t0);
    assert_eq!(
        s.on_request_sent(&request(0, ping()), t0),
        Err(RequestError::ZeroRequestId)
    );
    assert_eq!(
        s.on_request_sent(
            &v1::Request {
                request_id: 1,
                command: None
            },
            t0
        ),
        Err(RequestError::MissingCommand)
    );
    s.on_request_sent(&request(5, ping()), t0).expect("request");
    assert_eq!(
        s.on_request_sent(&request(5, ping()), t0),
        Err(RequestError::NotIncreasing { last: 5, got: 5 })
    );
    assert_eq!(
        s.on_request_sent(&request(4, ping()), t0),
        Err(RequestError::NotIncreasing { last: 5, got: 4 })
    );
    for id in 6..=36 {
        s.on_request_sent(&request(id, ping()), t0)
            .expect("request");
    }
    assert_eq!(s.outstanding_requests(), 32);
    assert_eq!(
        s.on_request_sent(&request(37, ping()), t0),
        Err(RequestError::TooManyOutstanding(32))
    );
    // A receiver set with an ID of 0, an ID twice, or more than 16.
    let mut s = established(t0);
    let many: Vec<u32> = (1..=17).collect();
    for ids in [&[0][..], &[1, 2, 1][..], &many[..]] {
        assert_eq!(
            s.on_request_sent(&request(1, configure(ids)), t0),
            Err(RequestError::InvalidReceiverSet)
        );
    }
    assert!(!s.is_receiver_configured(1, t0));
    for e in [
        RequestError::InvalidReceiverSet,
        RequestError::NotEstablished,
        RequestError::ZeroRequestId,
        RequestError::NotIncreasing { last: 1, got: 1 },
        RequestError::TooManyOutstanding(32),
        RequestError::MissingCommand,
        RequestError::ChannelInUse(1),
        RequestError::SubscriptionInUse(1),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn session_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Session>();
}
