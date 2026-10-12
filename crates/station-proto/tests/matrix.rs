// SPDX-License-Identifier: MIT
//! Matrices: version negotiation (§3) and response matching (§4/§5: every
//! command × every payload type × OK / error).

// Test tables are long by nature.
#![allow(clippy::too_many_lines, clippy::similar_names, clippy::type_complexity)]

mod common;

use std::collections::HashSet;
use std::time::Instant;

use common::*;
use station_proto::v1::{self, ProtocolVersion, request::Command, response::Payload};
use station_proto::validate::{
    CommandKind, DropReason, MessageKind, PayloadKind, Session, Unsupported, Verdict,
    ViolationKind, negotiate, negotiate_against, negotiate_welcome, negotiate_welcome_against,
};
use station_proto::{PROTOCOL_VERSION, v1::station_message::Body};

fn v(major: u32, minor: u32) -> ProtocolVersion {
    ProtocolVersion { major, minor }
}

#[test]
fn protocol_version_is_two_zero() {
    assert_eq!(PROTOCOL_VERSION, v(2, 0));
    assert_eq!(station_proto::limits::SUPPORTED_MAJORS, [2]);
}

#[test]
fn hub_negotiation_matrix() {
    // (hub version, supported majors, station version, expected)
    let cases: &[(
        ProtocolVersion,
        &[u32],
        Option<ProtocolVersion>,
        Option<ProtocolVersion>,
    )] = &[
        (v(1, 0), &[1], Some(v(1, 0)), Some(v(1, 0))),
        (v(1, 0), &[1], Some(v(1, 7)), Some(v(1, 0))),
        (v(1, 0), &[1], Some(v(0, 9)), None),
        (v(1, 0), &[1], Some(v(2, 0)), None),
        (v(1, 0), &[1], None, None),
        (v(1, 3), &[1], Some(v(1, 1)), Some(v(1, 1))),
        (v(1, 3), &[1], Some(v(1, 3)), Some(v(1, 3))),
        (v(1, 3), &[1], Some(v(1, 9)), Some(v(1, 3))),
        // Migration window: hub at 2.1 still accepts major 1, at the station's minor.
        (v(2, 1), &[1, 2], Some(v(1, 4)), Some(v(1, 4))),
        (v(2, 1), &[1, 2], Some(v(2, 0)), Some(v(2, 0))),
        (v(2, 1), &[1, 2], Some(v(2, 5)), Some(v(2, 1))),
        (v(2, 1), &[2], Some(v(1, 4)), None),
        (v(2, 1), &[1, 2], Some(v(3, 0)), None),
    ];
    for &(ours, majors, station, expected) in cases {
        let hello = v1::Hello {
            protocol_version: station,
            ..hello_with(0, 0)
        };
        let got = negotiate_against(&hello, ours, majors);
        match expected {
            Some(e) => assert_eq!(got, Ok(e), "{ours:?} {majors:?} {station:?}"),
            None => assert_eq!(
                got,
                Err(Unsupported { peer: station }),
                "{ours:?} {station:?}"
            ),
        }
    }
    assert_eq!(negotiate(&hello_with(2, 0)), Ok(v(2, 0)));
    assert_eq!(negotiate(&hello_with(2, 4)), Ok(v(2, 0)));
    // Major 2 only: an older or newer major is told to update.
    for (major, minor) in [(1, 0), (1, 1), (3, 0), (0, 9)] {
        assert_eq!(
            negotiate(&hello_with(major, minor)),
            Err(Unsupported {
                peer: Some(v(major, minor))
            })
        );
    }
    assert_eq!(
        Unsupported {
            peer: Some(v(3, 1))
        }
        .to_string(),
        "unsupported protocol version 3.1"
    );
    assert_eq!(
        Unsupported { peer: None }.to_string(),
        "unsupported protocol version (none)"
    );
}

#[test]
fn station_welcome_matrix() {
    let welcome = |pv| v1::Welcome {
        protocol_version: pv,
        ..Default::default()
    };
    let cases: &[(ProtocolVersion, Option<ProtocolVersion>, bool)] = &[
        (v(1, 0), Some(v(1, 0)), true),
        (v(1, 0), Some(v(1, 1)), false), // hub must pick the lower minor
        (v(1, 0), Some(v(2, 0)), false),
        (v(1, 0), Some(v(0, 0)), false),
        (v(1, 0), None, false),
        (v(1, 4), Some(v(1, 2)), true),
        (v(1, 4), Some(v(1, 4)), true),
        (v(1, 4), Some(v(1, 5)), false),
    ];
    for &(ours, hub, ok) in cases {
        let got = negotiate_welcome_against(&welcome(hub), ours);
        assert_eq!(got.is_ok(), ok, "{ours:?} {hub:?}");
        if ok {
            assert_eq!(got, Ok(hub.unwrap_or_default()));
        }
    }
    assert!(negotiate_welcome(&welcome(Some(v(2, 0)))).is_ok());
    assert!(negotiate_welcome(&welcome(Some(v(2, 1)))).is_err());
    assert!(negotiate_welcome(&welcome(Some(v(1, 1)))).is_err());
    assert!(negotiate_welcome(&welcome(Some(v(3, 0)))).is_err());
}

#[test]
fn session_rejects_unsupported_versions_without_a_strike() {
    let t0 = Instant::now();
    for station in [v(0, 1), v(1, 0), v(1, 1), v(3, 0), v(9, 9)] {
        let mut s = Session::new(t0);
        let m = msg(Body::Hello(hello_with(station.major, station.minor)));
        let Verdict::Violation(kind) = s.check(&m, t0) else {
            panic!("expected violation");
        };
        assert_eq!(
            kind,
            ViolationKind::UnsupportedVersion {
                major: station.major,
                minor: station.minor
            }
        );
        assert!(!kind.is_strike());
        assert_eq!(
            kind.goodbye_reason(),
            v1::goodbye::Reason::UnsupportedVersion
        );
        assert_eq!(s.negotiated_version(), None);
    }
    // A newer minor of the same major is accepted at our minor.
    let mut s = Session::new(t0);
    assert_eq!(
        s.check(&msg(Body::Hello(hello_with(2, 9))), t0),
        Verdict::Accept
    );
    assert_eq!(s.negotiated_version(), Some(v(2, 0)));
}

fn command(kind: CommandKind) -> Command {
    match kind {
        CommandKind::GetCapabilities => Command::GetCapabilities(v1::GetCapabilities {}),
        CommandKind::OpenChannel => open_channel(7, 20),
        CommandKind::UpdateChannel => Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 7,
            ..Default::default()
        }),
        CommandKind::CloseChannel => close_channel(7),
        CommandKind::SetFrontend => Command::SetFrontend(v1::SetFrontend {
            receiver_id: RX,
            ..Default::default()
        }),
        CommandKind::SubscribeWaterfall => subscribe(7, 1024, 100),
        CommandKind::UnsubscribeWaterfall => unsubscribe(7),
        CommandKind::Ping => Command::Ping(v1::Ping {
            nonce: 7,
            hub_time_unix_ms: 0,
        }),
        CommandKind::AdminOp => Command::AdminOp(v1::AdminOp {
            kind: 2,
            receiver_id: Some(RX),
        }),
        CommandKind::StartMeasurements => start_measurements(7),
        CommandKind::StopMeasurements => {
            Command::StopMeasurements(v1::StopMeasurements { measurement_id: 7 })
        }
        CommandKind::GetHardware => Command::GetHardware(v1::GetHardware {}),
        CommandKind::ConfigureReceivers => configure(&[RX]),
    }
}

/// A statelessly valid payload of `kind` that echoes id/nonce 7 and
/// receiver [`RX`].
fn payload(kind: PayloadKind) -> Payload {
    match kind {
        PayloadKind::Capabilities => Payload::Capabilities(capabilities()),
        PayloadKind::ChannelOpened => channel_opened(7, 20),
        PayloadKind::ChannelUpdated => channel_updated(7),
        PayloadKind::FrontendState => frontend_state(),
        PayloadKind::WaterfallSubscribed => waterfall_subscribed(7, 1024, 100),
        PayloadKind::Pong => pong(7),
        PayloadKind::MeasurementsStarted => measurements_started(7),
        PayloadKind::Hardware => Payload::Hardware(hardware()),
        PayloadKind::ReceiversConfigured => receivers_configured(&[RX]),
    }
}

#[test]
fn response_matching_matrix() {
    let t0 = Instant::now();
    let codes = [
        v1::ResultCode::Ok,
        v1::ResultCode::InvalidArgument,
        v1::ResultCode::OutOfRange,
        v1::ResultCode::ResourceExhausted,
        v1::ResultCode::Unsupported,
        v1::ResultCode::PermissionDenied,
        v1::ResultCode::NotFound,
        v1::ResultCode::AlreadyExists,
        v1::ResultCode::BackendError,
    ];
    let payloads: Vec<Option<PayloadKind>> = std::iter::once(None)
        .chain(PayloadKind::ALL.into_iter().map(Some))
        .collect();
    let mut accepted = 0;
    let mut rejected = 0;
    for cmd in CommandKind::ALL {
        for code in codes {
            for &p in &payloads {
                let mut s = established(t0);
                s.on_request_sent(&request(1, command(cmd)), t0)
                    .expect("request");
                let verdict = s.check(&response(1, code as i32, p.map(payload)), t0);
                let ok = code == v1::ResultCode::Ok;
                let expected = if ok { cmd.ok_payload() } else { None };
                if p == expected {
                    assert_eq!(verdict, Verdict::Accept, "{cmd} {code:?} {p:?}");
                    accepted += 1;
                } else {
                    assert_eq!(
                        verdict,
                        Verdict::Violation(ViolationKind::PayloadMismatch {
                            request_id: 1,
                            command: cmd,
                            code: code as i32,
                            payload: p,
                        }),
                        "{cmd} {code:?} {p:?}"
                    );
                    rejected += 1;
                }
                assert_eq!(s.outstanding_requests(), 0);
            }
        }
    }
    // 13 commands × 9 codes, one right answer each out of 10.
    assert_eq!(accepted, 13 * 9);
    assert_eq!(rejected, 13 * 9 * 10 - 13 * 9);
}

#[test]
fn ok_payload_table() {
    let expected = [
        (
            CommandKind::GetCapabilities,
            Some(PayloadKind::Capabilities),
        ),
        (CommandKind::OpenChannel, Some(PayloadKind::ChannelOpened)),
        (
            CommandKind::UpdateChannel,
            Some(PayloadKind::ChannelUpdated),
        ),
        (CommandKind::CloseChannel, None),
        (CommandKind::SetFrontend, Some(PayloadKind::FrontendState)),
        (
            CommandKind::SubscribeWaterfall,
            Some(PayloadKind::WaterfallSubscribed),
        ),
        (CommandKind::UnsubscribeWaterfall, None),
        (CommandKind::Ping, Some(PayloadKind::Pong)),
        (CommandKind::AdminOp, None),
        (
            CommandKind::StartMeasurements,
            Some(PayloadKind::MeasurementsStarted),
        ),
        (CommandKind::StopMeasurements, None),
        (CommandKind::GetHardware, Some(PayloadKind::Hardware)),
        (
            CommandKind::ConfigureReceivers,
            Some(PayloadKind::ReceiversConfigured),
        ),
    ];
    assert_eq!(expected.len(), CommandKind::ALL.len());
    for (cmd, p) in expected {
        assert_eq!(cmd.ok_payload(), p, "{cmd}");
        assert_eq!(CommandKind::of(&command(cmd)), cmd);
        let timeout = if cmd == CommandKind::GetCapabilities {
            10
        } else {
            5
        };
        assert_eq!(cmd.timeout(), secs(timeout));
    }
    for p in PayloadKind::ALL {
        assert_eq!(PayloadKind::of(&payload(p)), p);
    }
    let names: HashSet<_> = CommandKind::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(names.len(), 13);
}

#[test]
fn violation_kind_metadata() {
    let all = [
        ViolationKind::MessageTooLarge { len: 1, max: 0 },
        ViolationKind::Decode,
        ViolationKind::EmptyMessage,
        ViolationKind::FirstMessageNotHello {
            got: MessageKind::Audio,
        },
        ViolationKind::DuplicateHello,
        ViolationKind::MissingField { field: "f" },
        ViolationKind::StringTooLong {
            field: "f",
            len: 2,
            max: 1,
        },
        ViolationKind::TooManyEntries {
            field: "f",
            len: 2,
            max: 1,
        },
        ViolationKind::UnknownEnumValue {
            field: "f",
            value: 9,
            minor: 0,
        },
        ViolationKind::UnspecifiedEnum { field: "f" },
        ViolationKind::NonFiniteFloat { field: "f" },
        ViolationKind::OutOfBounds { field: "f" },
        ViolationKind::InvalidRange { field: "f" },
        ViolationKind::TunableOutsideHardware { receiver_id: 1 },
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Status,
            receiver_id: 9,
        },
        ViolationKind::DuplicateReceiver {
            field: "status.receivers.receiver_id",
            receiver_id: 1,
        },
        ViolationKind::UnsolicitedResponse { request_id: 1 },
        ViolationKind::PayloadMismatch {
            request_id: 1,
            command: CommandKind::Ping,
            code: 1,
            payload: None,
        },
        ViolationKind::ResponseFieldMismatch {
            request_id: 1,
            command: CommandKind::Ping,
            field: "pong.nonce",
        },
        ViolationKind::UnknownChannel {
            kind: MessageKind::Signal,
            channel_id: 1,
        },
        ViolationKind::UnknownSubscription { subscription_id: 1 },
        ViolationKind::AudioPayloadSize { len: 0 },
        ViolationKind::AudioRateSustained { channel_id: 1 },
        ViolationKind::WaterfallPayloadTooLarge { len: 1 },
        ViolationKind::WaterfallDecompression,
        ViolationKind::WaterfallLength {
            expected: 1,
            actual: 2,
        },
        ViolationKind::WaterfallRate { subscription_id: 1 },
        ViolationKind::ResponseTimeouts { consecutive: 3 },
        ViolationKind::UnknownMeasurement {
            kind: MessageKind::Noise,
            measurement_id: 1,
        },
        ViolationKind::MeasurementRate {
            kind: MessageKind::Noise,
            measurement_id: 1,
        },
        ViolationKind::UnsupportedVersion { major: 1, minor: 1 },
    ];
    let codes: HashSet<_> = all.iter().map(ViolationKind::code).collect();
    assert_eq!(codes.len(), all.len(), "codes are unique");
    for k in &all {
        assert!(!k.to_string().is_empty());
        assert!(k.code().chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        let unsupported = matches!(k, ViolationKind::UnsupportedVersion { .. });
        assert_eq!(k.is_strike(), !unsupported);
        let reason = if unsupported {
            v1::goodbye::Reason::UnsupportedVersion
        } else {
            v1::goodbye::Reason::ProtocolViolation
        };
        assert_eq!(k.goodbye_reason(), reason);
    }
    assert!(
        ViolationKind::PayloadMismatch {
            request_id: 4,
            command: CommandKind::OpenChannel,
            code: 1,
            payload: Some(PayloadKind::Pong),
        }
        .to_string()
        .contains("open_channel")
    );
    let kinds = [
        (
            Body::Hello(v1::Hello::default()),
            MessageKind::Hello,
            "hello",
        ),
        (
            Body::Response(v1::Response::default()),
            MessageKind::Response,
            "response",
        ),
        (
            Body::Audio(v1::AudioFrame::default()),
            MessageKind::Audio,
            "audio",
        ),
        (
            Body::Waterfall(v1::WaterfallRow::default()),
            MessageKind::Waterfall,
            "waterfall",
        ),
        (
            Body::Signal(v1::ChannelSignal::default()),
            MessageKind::Signal,
            "signal",
        ),
        (
            Body::Status(v1::StationStatus::default()),
            MessageKind::Status,
            "status",
        ),
        (
            Body::Event(v1::Event::default()),
            MessageKind::Event,
            "event",
        ),
        (
            Body::Noise(v1::NoiseFloorReport::default()),
            MessageKind::Noise,
            "noise",
        ),
    ];
    for (body, kind, name) in kinds {
        assert_eq!(MessageKind::of(&body), kind);
        assert_eq!(kind.to_string(), name);
    }
    let d = DropReason::RateLimited {
        kind: MessageKind::Event,
    };
    assert!(format!("{d:?}").contains("Event"));
}
