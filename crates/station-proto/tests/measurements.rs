// SPDX-License-Identifier: MIT
//! Measurements (§5.2): every rule of the table, per receiver, and the
//! wall-clock plausibility checks.

mod common;

use std::time::Instant;

use common::*;
use station_proto::v1::{self, request::Command, response::Payload, station_message::Body};
use station_proto::validate::{
    CommandKind, DropReason, MessageKind, RequestError, RequestRejection, Verdict, ViolationKind,
    check_request, is_band_key, is_measuring_minute, sanitize_capabilities,
};

use v1::MeasurementDuty::{Continuous, Idle};

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
fn capabilities_keep_and_clamp_the_limits() {
    let mut caps = capabilities();
    caps.measurements = Some(v1::MeasurementLimits {
        max_noise_bands: 4,
        method_version: 1,
    });
    let kept = sanitize_capabilities(&caps, 0).expect("valid");
    assert_eq!(kept.capabilities.measurements, caps.measurements);
    assert!(kept.clamped.is_empty());
    caps.measurements = Some(v1::MeasurementLimits {
        max_noise_bands: 99,
        method_version: 1,
    });
    let s = sanitize_capabilities(&caps, 0).expect("valid");
    assert_eq!(
        s.capabilities.measurements.map(|m| m.max_noise_bands),
        Some(32)
    );
    assert_eq!(s.clamped, ["capabilities.measurements.max_noise_bands"]);
}

#[test]
fn station_side_request_checks() {
    let start = request(1, start_measurements(9));
    assert_eq!(check_request(&start, 0), Ok(CommandKind::StartMeasurements));
    let stop = request(
        2,
        Command::StopMeasurements(v1::StopMeasurements { measurement_id: 9 }),
    );
    assert_eq!(check_request(&stop, 0), Ok(CommandKind::StopMeasurements));
    assert_eq!(
        check_request(&request(3, start_measurements(0)), 0),
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
        // Band 1 was asked of RX.
        (
            started_with(|m| {
                m.noise_bands[0].receiver_id = RX2;
                m.receivers[0] = measured(RX2, Idle);
            }),
            "measurements_started.noise_bands.receiver_id",
        ),
        // A receiver no accepted band is on...
        (
            started_with(|m| m.receivers.push(measured(RX2, Idle))),
            "measurements_started.receivers.receiver_id",
        ),
        // ...a receiver twice...
        (
            started_with(|m| m.receivers.push(measured(RX, Continuous))),
            "measurements_started.receivers.receiver_id",
        ),
        // ...or an accepted band's receiver missing.
        (
            started_with(|m| m.receivers.clear()),
            "measurements_started.receivers",
        ),
        // A cadence faster than asked.
        (
            started_with(|m| m.receivers[0].noise_interval_ms = 60_000 - 1),
            "measurements_started.receivers.noise_interval_ms",
        ),
    ];
    for (payload, field) in cases {
        let mut s = established(t0);
        s.on_request_sent(&request(1, start_measurements(9)), t0)
            .expect("request");
        let v = s.check(&response(1, OK, payload), t0);
        match violation(&v) {
            // Stateless rules come first (a cadence outside the set).
            ViolationKind::ResponseFieldMismatch {
                request_id: 1,
                command: CommandKind::StartMeasurements,
                field: f,
            }
            | ViolationKind::OutOfBounds { field: f } => assert_eq!(*f, field),
            other => panic!("{field}: {other:?}"),
        }
    }
    // Asked for 2 minutes, answered 1.
    let mut s = established(t0);
    let mut slow = start_measurements(9);
    if let Command::StartMeasurements(m) = &mut slow {
        m.noise_interval_ms = 120_000;
    }
    s.on_request_sent(&request(1, slow), t0).expect("request");
    assert!(matches!(
        violation(&s.check(&response(1, OK, started_with(|_| {})), t0)),
        ViolationKind::ResponseFieldMismatch {
            field: "measurements_started.receivers.noise_interval_ms",
            ..
        }
    ));
    // A stretched cycle and a narrower range are fine.
    let mut s = established(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    let ok = started_with(|m| {
        m.receivers[0].noise_interval_ms = 120_000;
        m.noise_bands[0].ranges = vec![range(7_000_000, 7_050_000)];
    });
    assert_eq!(s.check(&response(1, OK, ok), t0), Verdict::Accept);
    // Both receivers, each with its own cadence and duty.
    let mut s = established(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(1, OK, both_started(9)), t0),
        Verdict::Accept
    );
    // Nothing accepted (no band inside a receiver) is a plan too.
    let mut s = established(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    let none = started_with(|m| {
        m.noise_bands.clear();
        m.receivers.clear();
    });
    assert_eq!(s.check(&response(1, OK, none), t0), Verdict::Accept);
}

/// Both bands accepted: band 1 on RX (idle, every minute) and band 2 on
/// RX2 (continuous, a 2-minute cycle).
#[allow(clippy::unnecessary_wraps, reason = "a response payload")]
fn both_started(measurement_id: u32) -> Option<Payload> {
    let Payload::MeasurementsStarted(mut m) = measurements_started(measurement_id) else {
        unreachable!()
    };
    m.noise_bands.push(v1::NoiseBand {
        band_id: 2,
        receiver_id: RX2,
        ranges: vec![range(14_000_000, 14_350_000)],
        key: "20m".into(),
    });
    let mut second = measured(RX2, Continuous);
    second.noise_interval_ms = 120_000;
    m.receivers.push(second);
    Some(Payload::MeasurementsStarted(m))
}

/// A session with plan 9 over both receivers.
fn with_both(t0: Instant) -> station_proto::validate::Session {
    let mut s = established(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    assert_eq!(
        s.check(&response(1, OK, both_started(9)), t0),
        Verdict::Accept
    );
    s
}

/// A valid report of RX2's band 2 for plan 9.
fn noise_rx2(window_start_unix_ms: i64) -> v1::StationMessage {
    let mut m = noise(9, window_start_unix_ms);
    if let Some(Body::Noise(n)) = m.body.as_mut() {
        n.receiver_id = RX2;
        n.frontend = Some(measuring_frontend(RX2));
        n.cycle_minutes = 2;
        n.bands[0].band_id = 2;
        n.bands[0].measured = Some(range(14_000_000, 14_350_000));
    }
    m
}

#[test]
fn each_receiver_reports_its_own_bands() {
    let t0 = Instant::now();
    let mut s = with_both(t0);
    assert_eq!(s.check(&noise(9, NOON), t0), Verdict::Accept);
    assert_eq!(s.check(&noise_rx2(NOON), t0), Verdict::Accept);
    // RX reporting RX2's band, and RX2 reporting RX's.
    let mut s = with_both(t0);
    assert_eq!(
        *violation(&s.check(&noise_with(|n| n.bands[0].band_id = 2), t0)),
        ViolationKind::OutOfBounds {
            field: "noise.bands.band_id"
        }
    );
    let mut wrong = noise_rx2(NOON);
    if let Some(Body::Noise(n)) = wrong.body.as_mut() {
        n.bands[0].band_id = 1;
        n.bands[0].measured = Some(range(7_000_000, 7_300_000));
    }
    let mut s = with_both(t0);
    assert_eq!(
        *violation(&s.check(&wrong, t0)),
        ViolationKind::OutOfBounds {
            field: "noise.bands.band_id"
        }
    );
    // Rates are per receiver: two reports each at once (burst 2), then RX's
    // third is dropped while RX2 is unaffected... and RX2's cadence is its
    // own (2 minutes: one per minute).
    let mut s = with_both(t0);
    for _ in 0..2 {
        assert_eq!(s.check(&noise(9, NOON), t0), Verdict::Accept);
        assert_eq!(s.check(&noise_rx2(NOON), t0), Verdict::Accept);
    }
    let rate = Verdict::Drop(DropReason::RateLimited {
        kind: MessageKind::Noise,
    });
    assert_eq!(s.check(&noise(9, NOON), t0), rate);
    assert_eq!(s.check(&noise_rx2(NOON), t0 + secs(31)), rate);
    assert_eq!(s.check(&noise(9, NOON), t0 + secs(31)), Verdict::Accept);
    assert_eq!(s.check(&noise_rx2(NOON), t0 + secs(61)), Verdict::Accept);
    // Duty is per receiver: RX idle skips minute 3, RX2 continuous doesn't.
    let mut s = with_both(t0);
    assert_eq!(
        s.check(&noise(9, NOON + 3 * MIN), t0),
        Verdict::Drop(DropReason::NotScheduled)
    );
    assert_eq!(s.check(&noise_rx2(NOON + 3 * MIN), t0), Verdict::Accept);
    // RX2 goes idle from minute 4; RX doesn't change.
    assert_eq!(
        s.check(&duty_event_for(RX2, 9, Idle, NOON + 4 * MIN), t0),
        Verdict::Accept
    );
    assert_eq!(
        s.check(&noise_rx2(NOON + 4 * MIN), t0 + secs(60)),
        Verdict::Drop(DropReason::NotScheduled)
    );
    assert_eq!(
        s.check(&noise(9, NOON + 4 * MIN), t0 + secs(60)),
        Verdict::Drop(DropReason::NotScheduled)
    );
    // Duty-change rates are per receiver too.
    let mut s = with_both(t0);
    for _ in 0..2 {
        assert_eq!(
            s.check(&duty_event_for(RX, 9, Continuous, NOON), t0),
            Verdict::Accept
        );
    }
    assert!(matches!(
        s.check(&duty_event_for(RX, 9, Continuous, NOON), t0),
        Verdict::Drop(DropReason::RateLimited { .. })
    ));
    assert_eq!(
        s.check(&duty_event_for(RX2, 9, Idle, NOON), t0),
        Verdict::Accept
    );
}

#[test]
fn receivers_outside_the_plan() {
    let t0 = Instant::now();
    // The fixture plan measures RX only.
    let mut s = with_plan(t0, 9);
    assert_eq!(
        *violation(&s.check(&noise_rx2(NOON), t0)),
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Noise,
            receiver_id: RX2
        }
    );
    let mut s = with_plan(t0, 9);
    assert_eq!(
        *violation(&s.check(&duty_event_for(RX2, 9, Continuous, NOON), t0)),
        ViolationKind::UnknownReceiver {
            kind: MessageKind::Event,
            receiver_id: RX2
        }
    );
    // A duty change before the plan's answer: dropped, its receivers aren't
    // known yet.
    let mut s = established(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    assert_eq!(
        s.check(&duty_event(9, Continuous, NOON), t0),
        Verdict::Drop(DropReason::AwaitingMeasurements { measurement_id: 9 })
    );
}

#[test]
fn measurements_started_stateless_rules() {
    let t0 = Instant::now();
    let cases: Vec<(Option<Payload>, ViolationKind)> = vec![
        (
            started_with(|m| m.receivers[0].noise_interval_ms = 90_000),
            ViolationKind::OutOfBounds {
                field: "measurements_started.receivers.noise_interval_ms",
            },
        ),
        (
            started_with(|m| m.receivers[0].duty = 0),
            ViolationKind::UnspecifiedEnum {
                field: "measurements_started.receivers.duty",
            },
        ),
        (
            started_with(|m| m.receivers[0].duty = 7),
            ViolationKind::UnknownEnumValue {
                field: "measurements_started.receivers.duty",
                value: 7,
                minor: 0,
            },
        ),
        (
            started_with(|m| m.receivers[0].frontend = None),
            ViolationKind::MissingField {
                field: "measurements_started.receivers.frontend",
            },
        ),
        (
            started_with(|m| m.receivers[0].frontend = Some(measuring_frontend(RX2))),
            ViolationKind::OutOfBounds {
                field: "measurements_started.receivers.frontend.receiver_id",
            },
        ),
        (
            started_with(|m| {
                m.receivers = (1..=17).map(|id| measured(id, Idle)).collect();
            }),
            ViolationKind::TooManyEntries {
                field: "measurements_started.receivers",
                len: 17,
                max: 16,
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
        let mut s = established(t0);
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
    let mut s = established(t0);
    s.on_request_sent(&request(1, start_measurements(9)), t0)
        .expect("request");
    let out_of_range = v1::ResultCode::OutOfRange as i32;
    assert_eq!(
        s.check(&response(1, out_of_range, None), t0),
        Verdict::Accept
    );
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
    let mut s = established(t0);
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
            noise_with(|n| n.frontend = Some(measuring_frontend(RX2))),
            ViolationKind::OutOfBounds {
                field: "noise.frontend.receiver_id",
            },
        ),
        (
            noise_with(|n| {
                n.receiver_id = 0;
                n.frontend = Some(measuring_frontend(0));
            }),
            ViolationKind::UnknownReceiver {
                kind: MessageKind::Noise,
                receiver_id: 0,
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
        e.receiver_id = None;
    }
    assert_eq!(
        *violation(&s.check(&m, t0)),
        ViolationKind::MissingField {
            field: "event.receiver_id"
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
