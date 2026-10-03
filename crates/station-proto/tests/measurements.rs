// SPDX-License-Identifier: MIT
//! Station protocol 1.1, measurements (§5.2): every rule of the table, the
//! 1.0 compatibility, and the wall-clock plausibility checks.

mod common;

use std::time::Instant;

use common::*;
use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{
    CommandKind, DropReason, MessageKind, RequestError, RequestRejection, Verdict, ViolationKind,
    check_request, is_band_key, is_measuring_minute, sanitize_capabilities,
};

/// 2026-10-02 12:00:00 UTC: a measuring minute.
const NOON: i64 = 1_790_942_400_000;
const MIN: i64 = 60_000;

fn violation(v: &Verdict) -> &ViolationKind {
    match v {
        Verdict::Violation(k) => k,
        other => panic!("expected a violation, got {other:?}"),
    }
}

fn noise_with(f: impl FnOnce(&mut v1::NoiseFloorReport)) -> v1::StationMessage {
    let mut m = noise(9, NOON);
    if let Some(Body::Noise(n)) = m.body.as_mut() {
        f(n);
    }
    m
}

#[allow(clippy::unnecessary_wraps, reason = "a response payload")]
fn started_with(f: impl FnOnce(&mut v1::MeasurementsStarted)) -> Option<Payload> {
    let Payload::MeasurementsStarted(mut m) = measurements_started(9) else {
        unreachable!()
    };
    f(&mut m);
    Some(Payload::MeasurementsStarted(m))
}

#[test]
fn helpers() {
    assert!(is_band_key("40m") && is_band_key("23cm") && is_band_key("160m"));
    assert!(!is_band_key("") && !is_band_key("40M") && !is_band_key("123456789"));
    assert!(!is_band_key("4 0"));
    assert!(is_measuring_minute(NOON));
    assert!(is_measuring_minute(NOON + 59_999));
    assert!(!is_measuring_minute(NOON + MIN));
    assert!(is_measuring_minute(NOON + 5 * MIN));
    assert!(is_measuring_minute(-5 * MIN));
    assert!(!is_measuring_minute(-MIN));
}

#[test]
fn a_1_0_session_has_no_measurements() {
    let t0 = Instant::now();
    let mut s = established(t0);
    // The hub must not send a plan.
    assert_eq!(
        s.on_request_sent(&request(1, start_measurements(9)), t0),
        Err(RequestError::NotInMinor(CommandKind::StartMeasurements))
    );
    // A noise report is a message 1.0 does not have.
    assert_eq!(
        *violation(&s.check(&noise(9, NOON), t0)),
        ViolationKind::MessageNotInMinor {
            kind: MessageKind::Noise,
            minor: 0
        }
    );
    // Measurement event kinds are unknown in 1.0.
    let mut s = established(t0);
    assert!(matches!(
        violation(&s.check(&stopped_event(9), t0)),
        ViolationKind::UnknownEnumValue {
            field: "event.kind",
            ..
        }
    ));
    // `Capabilities.measurements` is a field 1.0 ignores.
    let mut caps = capabilities();
    caps.measurements = Some(v1::MeasurementLimits {
        enabled: true,
        max_noise_bands: 4,
        method_version: 1,
    });
    let clean = sanitize_capabilities(&caps, 0).expect("valid");
    assert!(clean.capabilities.measurements.is_none());
    let kept = sanitize_capabilities(&caps, 1).expect("valid");
    assert_eq!(kept.capabilities.measurements, caps.measurements);
}

#[test]
fn capabilities_clamp_the_band_count() {
    let mut caps = capabilities();
    caps.measurements = Some(v1::MeasurementLimits {
        enabled: true,
        max_noise_bands: 99,
        method_version: 1,
    });
    let s = sanitize_capabilities(&caps, 1).expect("valid");
    assert_eq!(
        s.capabilities.measurements.map(|m| m.max_noise_bands),
        Some(32)
    );
    assert_eq!(s.clamped, ["capabilities.measurements.max_noise_bands"]);
}

#[test]
fn station_side_request_checks() {
    let start = request(1, start_measurements(9));
    assert_eq!(
        check_request(&start, 0),
        Err(RequestRejection::Unsupported {
            field: "request.command"
        })
    );
    assert_eq!(check_request(&start, 1), Ok(CommandKind::StartMeasurements));
    let stop = request(
        2,
        Command::StopMeasurements(v1::StopMeasurements { measurement_id: 9 }),
    );
    assert!(check_request(&stop, 0).is_err());
    assert_eq!(check_request(&stop, 1), Ok(CommandKind::StopMeasurements));
    assert_eq!(
        check_request(&request(3, start_measurements(0)), 1),
        Err(RequestRejection::InvalidArgument {
            field: "start_measurements.measurement_id"
        })
    );
}

#[test]
fn one_plan_per_id() {
    let t0 = Instant::now();
    let mut s = with_plan(t0, 9);
    assert!(s.is_measurement_open(9, t0));
    assert_eq!(
        s.on_request_sent(&request(2, start_measurements(9)), t0),
        Err(RequestError::MeasurementInUse(9))
    );
    // A new id is fine.
    s.on_request_sent(&request(3, start_measurements(10)), t0)
        .expect("another plan");
}

#[test]
fn measurements_started_must_echo_the_request() {
    let t0 = Instant::now();
    let cases: Vec<(Option<Payload>, &str)> = vec![
        (
            started_with(|m| m.measurement_id = 8),
            "measurements_started.measurement_id",
        ),
        (
            started_with(|m| m.noise_bands[0].band_id = 3),
            "measurements_started.noise_bands.band_id",
        ),
        (
            started_with(|m| m.noise_bands[0].ranges[1].end_hz = 7_350_000),
            "measurements_started.noise_bands.ranges",
        ),
        (
            started_with(|m| {
                m.noise_bands[0].ranges = vec![range(7_090_000, 7_110_000)];
            }),
            "measurements_started.noise_bands.ranges",
        ),
    ];
    for (payload, field) in cases {
        let mut s = established_1_1(t0);
        s.on_request_sent(&request(1, start_measurements(9)), t0)
            .expect("request");
        assert_eq!(
            *violation(&s.check(&response(1, OK, payload), t0)),
            ViolationKind::ResponseFieldMismatch {
                request_id: 1,
                command: CommandKind::StartMeasurements,
                field
            }
        );
    }
    // A stretched cycle and a narrower range are fine.
    let mut s = established_1_1(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    let ok = started_with(|m| {
        m.noise_interval_ms = 120_000;
        m.noise_bands[0].ranges = vec![range(7_000_000, 7_050_000)];
    });
    assert_eq!(s.check(&response(1, OK, ok), t0), Verdict::Accept);
}

#[test]
fn measurements_started_stateless_rules() {
    let t0 = Instant::now();
    let cases: Vec<(Option<Payload>, ViolationKind)> = vec![
        (
            started_with(|m| m.noise_interval_ms = 90_000),
            ViolationKind::OutOfBounds {
                field: "measurements_started.noise_interval_ms",
            },
        ),
        (
            started_with(|m| m.duty = 0),
            ViolationKind::UnspecifiedEnum {
                field: "measurements_started.duty",
            },
        ),
        (
            started_with(|m| m.duty = 7),
            ViolationKind::UnknownEnumValue {
                field: "measurements_started.duty",
                value: 7,
                minor: 1,
            },
        ),
        (
            started_with(|m| m.frontend = None),
            ViolationKind::MissingField {
                field: "measurements_started.frontend",
            },
        ),
        (
            started_with(|m| m.noise_bands[0].key = "40 m".into()),
            ViolationKind::OutOfBounds {
                field: "measurements_started.noise_bands.key",
            },
        ),
        (
            started_with(|m| m.noise_bands[0].ranges.reverse()),
            ViolationKind::InvalidRange {
                field: "measurements_started.noise_bands.ranges",
            },
        ),
        (
            started_with(|m| m.noise_bands[0].ranges.clear()),
            ViolationKind::MissingField {
                field: "measurements_started.noise_bands.ranges",
            },
        ),
        (
            started_with(|m| {
                let b = m.noise_bands[0].clone();
                m.noise_bands.push(b);
            }),
            ViolationKind::OutOfBounds {
                field: "measurements_started.noise_bands.band_id",
            },
        ),
    ];
    for (payload, expected) in cases {
        let mut s = established_1_1(t0);
        s.on_request_sent(&request(1, start_measurements(9)), t0)
            .expect("request");
        assert_eq!(
            *violation(&s.check(&response(1, OK, payload), t0)),
            expected
        );
    }
}

#[test]
fn a_refused_plan_is_gone() {
    let t0 = Instant::now();
    let mut s = established_1_1(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    let denied = v1::ResultCode::PermissionDenied as i32;
    assert_eq!(s.check(&response(1, denied, None), t0), Verdict::Accept);
    assert!(!s.is_measurement_open(9, t0));
    assert_eq!(
        *violation(&s.check(&noise(9, NOON), t0)),
        ViolationKind::UnknownMeasurement {
            kind: MessageKind::Noise,
            measurement_id: 9
        }
    );
}

#[test]
fn reports_before_the_answer_are_dropped() {
    let t0 = Instant::now();
    let mut s = established_1_1(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    assert_eq!(
        s.check(&noise(9, NOON), t0),
        Verdict::Drop(DropReason::AwaitingMeasurements { measurement_id: 9 })
    );
}

#[test]
#[allow(clippy::too_many_lines, reason = "one case per rule")]
fn report_rules() {
    let t0 = Instant::now();
    let cases: Vec<(v1::StationMessage, ViolationKind)> = vec![
        (
            noise_with(|n| n.bands[0].band_id = 2),
            ViolationKind::OutOfBounds {
                field: "noise.bands.band_id",
            },
        ),
        (
            noise_with(|n| {
                let b = n.bands[0];
                n.bands.push(b);
            }),
            ViolationKind::OutOfBounds {
                field: "noise.bands.band_id",
            },
        ),
        (
            noise_with(|n| n.bands[0].measured = Some(range(6_990_000, 7_100_000))),
            ViolationKind::InvalidRange {
                field: "noise.bands.measured",
            },
        ),
        (
            noise_with(|n| n.bands[0].measured = Some(range(7_100_000, 7_100_000))),
            ViolationKind::InvalidRange {
                field: "noise.bands.measured",
            },
        ),
        (
            noise_with(|n| n.bands[0].p25_db_hz = -140.0),
            ViolationKind::OutOfBounds {
                field: "noise.bands.quantiles",
            },
        ),
        (
            noise_with(|n| n.bands[0].p90_db_hz = 51.0),
            ViolationKind::OutOfBounds {
                field: "noise.bands.quantiles",
            },
        ),
        (
            noise_with(|n| n.bands[0].p10_db_hz = f32::NAN),
            ViolationKind::NonFiniteFloat {
                field: "noise.bands.quantiles",
            },
        ),
        (
            noise_with(|n| n.bands[0].samples = 1_000_001),
            ViolationKind::OutOfBounds {
                field: "noise.bands.samples",
            },
        ),
        (
            noise_with(|n| n.bands[0].resolution_bw_hz = 0.0),
            ViolationKind::OutOfBounds {
                field: "noise.bands.resolution_bw_hz",
            },
        ),
        (
            noise_with(|n| n.frontend = None),
            ViolationKind::MissingField {
                field: "noise.frontend",
            },
        ),
        (
            noise_with(|n| n.clock = None),
            ViolationKind::MissingField {
                field: "noise.clock",
            },
        ),
        (
            noise_with(|n| n.window_ms = 0),
            ViolationKind::OutOfBounds {
                field: "noise.window_ms",
            },
        ),
        (
            noise_with(|n| n.cycle_minutes = 3),
            ViolationKind::OutOfBounds {
                field: "noise.cycle_minutes",
            },
        ),
        (
            noise_with(|n| {
                n.cycle_minutes = 2;
                n.cycle_index = 2;
            }),
            ViolationKind::OutOfBounds {
                field: "noise.cycle_minutes",
            },
        ),
        (
            noise_with(|n| {
                n.bands = (1..=33)
                    .map(|band_id| v1::BandNoise {
                        band_id,
                        ..n.bands[0]
                    })
                    .collect();
            }),
            ViolationKind::TooManyEntries {
                field: "noise.bands",
                len: 33,
                max: 32,
            },
        ),
        (
            noise_with(|n| n.measurement_id = 4),
            ViolationKind::UnknownMeasurement {
                kind: MessageKind::Noise,
                measurement_id: 4,
            },
        ),
    ];
    for (m, expected) in cases {
        let mut s = with_plan(t0, 9);
        assert_eq!(*violation(&s.check(&m, t0)), expected);
    }
    // The valid one, and one without bands (nothing measurable that minute).
    let mut s = with_plan(t0, 9);
    assert_eq!(s.check(&noise(9, NOON), t0), Verdict::Accept);
    let mut s = with_plan(t0, 9);
    assert_eq!(
        s.check(&noise_with(|n| n.bands.clear()), t0),
        Verdict::Accept
    );
}

#[test]
fn report_times_against_the_hub_clock() {
    let t0 = Instant::now();
    let time = Verdict::Drop(DropReason::MeasurementTime {
        kind: MessageKind::Noise,
    });
    let now = NOON + MIN + 2_000;
    let cases = [
        (NOON, Verdict::Accept),
        (NOON + 1_000, time.clone()),
        (NOON - 15 * MIN, time.clone()),
        (NOON - 13 * MIN, Verdict::Accept),
        (NOON + 2 * MIN, time.clone()),
        (now + 5_000, time.clone()),
    ];
    for (start, expected) in cases {
        // Continuous duty, so only the clock decides.
        let mut s = with_plan(t0, 9);
        assert_eq!(
            s.check_at(
                &duty_event(9, v1::MeasurementDuty::Continuous, NOON - 20 * MIN),
                t0,
                None
            ),
            Verdict::Accept
        );
        assert_eq!(
            s.check_at(&noise(9, start), t0, Some(now)),
            expected,
            "{start}"
        );
    }
    // Without a clock only the minute alignment of the duty rule applies.
    let mut s = with_plan(t0, 9);
    assert_eq!(s.check(&noise(9, NOON - 60 * MIN), t0), Verdict::Accept);
}

#[test]
fn idle_duty_reports_only_measuring_minutes() {
    let t0 = Instant::now();
    // The fixture plan starts idle.
    let mut s = with_plan(t0, 9);
    assert_eq!(
        s.check(&noise(9, NOON + 3 * MIN), t0),
        Verdict::Drop(DropReason::NotScheduled)
    );
    let t1 = t0 + secs(60);
    assert_eq!(s.check(&noise(9, NOON + 5 * MIN), t1), Verdict::Accept);
    // Continuous from minute 6: every minute counts.
    assert_eq!(
        s.check(
            &duty_event(9, v1::MeasurementDuty::Continuous, NOON + 6 * MIN),
            t1
        ),
        Verdict::Accept
    );
    let t2 = t1 + secs(60);
    assert_eq!(s.check(&noise(9, NOON + 7 * MIN), t2), Verdict::Accept);
    // Back to idle from minute 9: minute 8 still counts, minute 9 doesn't.
    let t3 = t2 + secs(60);
    assert_eq!(
        s.check(
            &duty_event(9, v1::MeasurementDuty::Idle, NOON + 9 * MIN),
            t3
        ),
        Verdict::Accept
    );
    assert_eq!(s.check(&noise(9, NOON + 8 * MIN), t3), Verdict::Accept);
    let t4 = t3 + secs(60);
    assert_eq!(
        s.check(&noise(9, NOON + 9 * MIN), t4),
        Verdict::Drop(DropReason::NotScheduled)
    );
}

#[test]
fn noise_rate() {
    let t0 = Instant::now();
    let mut s = with_plan(t0, 9);
    assert_eq!(s.check(&noise(9, NOON), t0), Verdict::Accept);
    assert_eq!(s.check(&noise(9, NOON), t0), Verdict::Accept);
    assert_eq!(
        s.check(&noise(9, NOON), t0),
        Verdict::Drop(DropReason::RateLimited {
            kind: MessageKind::Noise
        })
    );
    // One per 30 s refills; a flood every 10 s is sustained after 60 s.
    let mut verdict = Verdict::Accept;
    for i in 1..=8 {
        verdict = s.check(&noise(9, NOON), t0 + secs(10 * i));
        if matches!(verdict, Verdict::Violation(_)) {
            break;
        }
    }
    assert_eq!(
        verdict,
        Verdict::Violation(ViolationKind::MeasurementRate {
            kind: MessageKind::Noise,
            measurement_id: 9
        })
    );
    // An honest station, one a minute, never trips it.
    let mut s = with_plan(t0, 9);
    for i in 0..30 {
        assert_eq!(s.check(&noise(9, NOON), t0 + secs(60 * i)), Verdict::Accept);
    }
}

#[test]
fn duty_events() {
    let t0 = Instant::now();
    let now = NOON + 30_000;
    let continuous = v1::MeasurementDuty::Continuous;
    let time = Verdict::Drop(DropReason::MeasurementTime {
        kind: MessageKind::Event,
    });
    for (from, expected) in [
        (NOON + MIN, Verdict::Accept),
        (NOON, Verdict::Accept),
        (NOON - MIN, time.clone()),
        (NOON - 2 * MIN, time.clone()),
        (NOON + 2 * MIN, time.clone()),
        (NOON + 1_000, time.clone()),
    ] {
        let mut s = with_plan(t0, 9);
        assert_eq!(
            s.check_at(&duty_event(9, continuous, from), t0, Some(now)),
            expected,
            "{from}"
        );
    }
    // Missing fields and unknown plans are violations.
    let mut s = with_plan(t0, 9);
    let mut m = duty_event(9, continuous, NOON);
    if let Some(Body::Event(e)) = m.body.as_mut() {
        e.effective_unix_ms = None;
    }
    assert_eq!(
        *violation(&s.check(&m, t0)),
        ViolationKind::MissingField {
            field: "event.effective_unix_ms"
        }
    );
    let mut s = with_plan(t0, 9);
    assert_eq!(
        *violation(&s.check(&duty_event(8, continuous, NOON), t0)),
        ViolationKind::UnknownMeasurement {
            kind: MessageKind::Event,
            measurement_id: 8
        }
    );
    let mut s = with_plan(t0, 9);
    let mut m = duty_event(9, continuous, NOON);
    if let Some(Body::Event(e)) = m.body.as_mut() {
        e.measurement_duty = Some(0);
    }
    assert_eq!(
        *violation(&s.check(&m, t0)),
        ViolationKind::UnspecifiedEnum {
            field: "event.measurement_duty"
        }
    );
    // Rate: burst 2, then dropped; flapping for 10 minutes is a violation.
    let mut s = with_plan(t0, 9);
    assert_eq!(
        s.check(&duty_event(9, continuous, NOON), t0),
        Verdict::Accept
    );
    assert_eq!(
        s.check(&duty_event(9, continuous, NOON), t0),
        Verdict::Accept
    );
    assert_eq!(
        s.check(&duty_event(9, continuous, NOON), t0),
        Verdict::Drop(DropReason::RateLimited {
            kind: MessageKind::Event
        })
    );
    let mut last = Verdict::Accept;
    for i in 1..=200 {
        last = s.check(&duty_event(9, continuous, NOON), t0 + secs(5 * i));
        if matches!(last, Verdict::Violation(_)) {
            break;
        }
    }
    assert_eq!(
        last,
        Verdict::Violation(ViolationKind::MeasurementRate {
            kind: MessageKind::Event,
            measurement_id: 9
        })
    );
}

#[test]
fn stopping_closes_with_a_grace() {
    let t0 = Instant::now();
    // The hub stops it.
    let mut s = with_plan(t0, 9);
    s.on_request_sent(
        &request(
            2,
            Command::StopMeasurements(v1::StopMeasurements { measurement_id: 9 }),
        ),
        t0,
    )
    .expect("stop");
    assert!(!s.is_measurement_open(9, t0));
    assert_eq!(
        s.check(&noise(9, NOON), t0 + secs(4)),
        Verdict::Drop(DropReason::RecentlyClosed {
            kind: MessageKind::Noise,
            id: 9
        })
    );
    assert!(matches!(
        s.check(&noise(9, NOON), t0 + secs(6)),
        Verdict::Violation(ViolationKind::UnknownMeasurement { .. })
    ));
    // The station stops it.
    let mut s = with_plan(t0, 9);
    assert_eq!(s.check(&stopped_event(9), t0), Verdict::Accept);
    assert!(!s.is_measurement_open(9, t0));
    assert_eq!(
        s.check(&stopped_event(9), t0),
        Verdict::Drop(DropReason::RecentlyClosed {
            kind: MessageKind::Event,
            id: 9
        })
    );
    let mut m = stopped_event(9);
    if let Some(Body::Event(e)) = m.body.as_mut() {
        e.measurement_id = None;
    }
    let mut s = with_plan(t0, 9);
    assert_eq!(
        *violation(&s.check(&m, t0)),
        ViolationKind::MissingField {
            field: "event.measurement_id"
        }
    );
}

#[test]
fn status_counts_dropped_reports() {
    let t0 = Instant::now();
    let mut s = with_plan(t0, 9);
    let status = msg(Body::Status(v1::StationStatus {
        measurement_reports_dropped_total: 12,
        ..Default::default()
    }));
    assert_eq!(s.check(&status, t0), Verdict::Accept);
}
