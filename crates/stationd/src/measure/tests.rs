// SPDX-License-Identifier: GPL-3.0-only
//! The plan runner under a paused clock with the fake backend: the idle
//! schedule, duty changes, stops, the local switch and the pause rules.

use std::sync::{Arc, Mutex};

use station_backend::{Backend, FakeBackend, MeasurementOp};
use station_proto::v1;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::*;
use crate::config::LocalPolicy;
use crate::dispatch::{self, Media, RequestContext};
use crate::outbound::QueueLimits;

use v1::station_message::Body;

/// 2026-10-02 12:00 UTC, a measuring minute.
const NOON: i64 = 1_790_942_400_000;
const MIN: i64 = 60_000;

struct Rig {
    backend: FakeBackend,
    clock: ScaledClock,
    measurer: Arc<Measurer>,
    requests: RequestContext,
    seen: Arc<Mutex<Vec<(i64, v1::StationMessage)>>>,
    load: Arc<Mutex<f32>>,
    _collector: JoinHandle<()>,
}

fn rig_with(enabled: bool, backend: FakeBackend) -> Rig {
    // 11:58:30 UTC.
    let clock = ScaledClock::new(NOON - 90_000, 1);
    // The fake floor's daily curve follows the test's clock, not the host's.
    let backend = backend.with_wall_clock(Arc::new(move || clock.now_ms()));
    let drops = Arc::new(DropCounters::default());
    let outbound = Outbound::new(QueueLimits::default(), Arc::clone(&drops));
    let tables: Arc<Mutex<Tables>> = Arc::default();
    let load = Arc::new(Mutex::new(0.1f32));
    let host_load = Arc::clone(&load);
    let measurer = Measurer::new(MeasureDeps {
        backend: Arc::new(backend.clone()),
        outbound: Arc::clone(&outbound),
        tables: Arc::clone(&tables),
        drops,
        clock: Arc::new(clock),
        host: Arc::new(move || v1::HostHealth {
            load_avg_1m: *host_load.lock().expect("load"),
            cpu_count: 4,
            ..Default::default()
        }),
        clock_state: Arc::new(|| v1::ClockState {
            synchronized: true,
            max_error_ms: 0,
        }),
        enabled,
    });
    let requests = RequestContext {
        backend: Arc::new(backend.clone()),
        policy: Arc::new(LocalPolicy {
            measurements: enabled,
            ..LocalPolicy::default()
        }),
        minor: 1,
        tables,
        measurer: Some(Arc::clone(&measurer)),
    };
    let seen: Arc<Mutex<Vec<(i64, v1::StationMessage)>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let collector = tokio::spawn(async move {
        while let Some(m) = outbound.next().await {
            sink.lock().expect("seen").push((clock.now_ms(), m));
        }
    });
    Rig {
        backend,
        clock,
        measurer,
        requests,
        seen,
        load,
        _collector: collector,
    }
}

fn rig() -> Rig {
    rig_with(true, FakeBackend::default())
}

fn forty() -> v1::StartMeasurements {
    v1::StartMeasurements {
        measurement_id: 7,
        noise_bands: vec![v1::NoiseBand {
            band_id: 3,
            ranges: vec![
                v1::FrequencyRange {
                    start_hz: 7_000_000,
                    end_hz: 7_098_000,
                },
                v1::FrequencyRange {
                    start_hz: 7_102_000,
                    end_hz: 7_300_000,
                },
            ],
            key: "40m".into(),
        }],
        noise_interval_ms: 60_000,
        bin_count: 1024,
        resolution_bw_hz: 500.0,
        dwell_ms: 4000,
        poll_interval_ms: 500,
    }
}

impl Rig {
    async fn until(&self, unix_ms: i64) {
        tokio::time::sleep_until(self.clock.instant_at(unix_ms)).await;
    }

    fn at(&self, i: Instant) -> i64 {
        self.clock.ms_at(i)
    }

    /// (time, op) of the measurement channel, in this clock's ms.
    fn ops(&self) -> Vec<(i64, MeasurementOp)> {
        self.backend
            .measurement_log()
            .into_iter()
            .map(|(i, op)| (self.at(i), op))
            .collect()
    }

    fn reports(&self) -> Vec<v1::NoiseFloorReport> {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .filter_map(|(_, m)| match &m.body {
                Some(Body::Noise(n)) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    fn events(&self) -> Vec<(i64, v1::Event)> {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .filter_map(|(t, m)| match &m.body {
                Some(Body::Event(e)) => Some((*t, e.clone())),
                _ => None,
            })
            .collect()
    }

    /// Opens a listener channel the way the hub would.
    async fn listen(&self, channel_id: u32) -> Media {
        let req = v1::Request {
            request_id: u64::from(channel_id),
            command: Some(v1::request::Command::OpenChannel(v1::OpenChannel {
                channel_id,
                frequency_hz: 7_100_000,
                mode: v1::Mode::Lsb as i32,
                low_cut_hz: -2800,
                high_cut_hz: -200,
                audio: Some(v1::AudioParams {
                    codec: v1::AudioCodec::Opus as i32,
                    bitrate_bps: 16_000,
                    sample_rate_hz: 12_000,
                    frame_duration_ms: 20,
                }),
                squelch_snr_db: None,
            })),
        };
        let (r, media) = dispatch::dispatch(&self.requests, req).await;
        assert_eq!(r.result.map(|r| r.code), Some(v1::ResultCode::Ok as i32));
        media.expect("audio")
    }

    async fn close(&self, channel_id: u32) {
        let req = v1::Request {
            request_id: 100 + u64::from(channel_id),
            command: Some(v1::request::Command::CloseChannel(v1::CloseChannel {
                channel_id,
            })),
        };
        let (r, _) = dispatch::dispatch(&self.requests, req).await;
        assert_eq!(r.result.map(|r| r.code), Some(v1::ResultCode::Ok as i32));
    }
}

fn duty_events(events: &[(i64, v1::Event)]) -> Vec<(i32, i64)> {
    events
        .iter()
        .filter(|(_, e)| e.kind == v1::event::Kind::MeasurementDuty as i32)
        .map(|(_, e)| {
            (
                e.measurement_duty.unwrap_or_default(),
                e.effective_unix_ms.unwrap_or_default(),
            )
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn idle_duty_measures_one_minute_in_five() {
    let r = rig();
    let started = r.measurer.start(forty()).await.expect("plan");
    assert_eq!(started.measurement_id, 7);
    assert_eq!(started.duty, v1::MeasurementDuty::Idle as i32);
    assert_eq!(started.noise_interval_ms, 60_000);
    assert_eq!(started.method_version, 1);
    assert_eq!(started.noise_bands.len(), 1);
    assert_eq!(started.noise_bands[0].ranges.len(), 2);
    assert_eq!(
        started.frontend.as_ref().map(|f| f.center_frequency_hz),
        Some(7_100_000)
    );
    r.until(NOON + 11 * MIN + 1_000).await;
    // The channel opens 2 s before each measuring minute and closes 0.5 s
    // after it: the front end runs 62.5 s in 300 s.
    let times: Vec<(i64, bool)> = r
        .ops()
        .into_iter()
        .map(|(t, op)| (t - NOON, matches!(op, MeasurementOp::Open { .. })))
        .collect();
    assert_eq!(
        times,
        [
            (-2_000, true),
            (60_500, false),
            (5 * MIN - 2_000, true),
            (6 * MIN + 500, false),
            (10 * MIN - 2_000, true),
            (11 * MIN + 500, false),
        ]
    );
    // One report per measuring minute, never for the others.
    let reports = r.reports();
    let starts: Vec<i64> = reports
        .iter()
        .map(|n| n.window_start_unix_ms - NOON)
        .collect();
    assert_eq!(starts, [0, 5 * MIN, 10 * MIN]);
    let n = &reports[0];
    assert_eq!(n.measurement_id, 7);
    assert_eq!(n.window_ms, 60_000);
    assert!(!n.partial);
    assert_eq!((n.cycle_minutes, n.cycle_index), (1, 0));
    assert_eq!(n.bands.len(), 1);
    let b = &n.bands[0];
    assert_eq!(b.band_id, 3);
    // 592 bins of 500 Hz (40 m minus the DC guard) × 120 spectra.
    assert_eq!(b.samples, 592 * 120);
    assert_eq!(
        b.measured,
        Some(v1::FrequencyRange {
            start_hz: 7_000_000,
            end_hz: 7_299_750
        })
    );
    // The fake floor at noon is about −123 dB/Hz.
    assert!(b.p10_db_hz < b.p25_db_hz && b.p25_db_hz < b.p50_db_hz && b.p50_db_hz < b.p90_db_hz);
    assert!((-128.0..-121.0).contains(&b.p50_db_hz), "{}", b.p50_db_hz);
    assert!(n.frontend.is_some() && n.clock.is_some());
    // No duty change.
    assert!(duty_events(&r.events()).is_empty());
}

#[tokio::test(start_paused = true)]
async fn listeners_make_it_continuous_from_the_next_minute() {
    let r = rig();
    r.measurer.start(forty()).await.expect("plan");
    // A listener at 11:59:20 makes 12:00 continuous; 12:01 is measured too.
    r.until(NOON - 40_000).await;
    let _audio = r.listen(1).await;
    r.until(NOON + 2 * MIN + 30_000).await;
    r.close(1).await;
    // Idle again from 12:03; the next measuring minute is 12:05.
    r.until(NOON + 6 * MIN + 1_000).await;
    let starts: Vec<i64> = r
        .reports()
        .iter()
        .map(|n| n.window_start_unix_ms - NOON)
        .collect();
    assert_eq!(starts, [0, MIN, 2 * MIN, 5 * MIN]);
    assert_eq!(
        duty_events(&r.events()),
        [
            (v1::MeasurementDuty::Continuous as i32, NOON),
            (v1::MeasurementDuty::Idle as i32, NOON + 3 * MIN),
        ]
    );
    // Open all through 12:00–12:03, closed 0.5 s into 12:03.
    let ops = r.ops();
    assert_eq!(ops[0].0, NOON - 2_000);
    assert_eq!(
        ops[1],
        (
            NOON + 3 * MIN + 500,
            MeasurementOp::Close { id: ops_id(&ops) }
        )
    );
}

fn ops_id(ops: &[(i64, MeasurementOp)]) -> u32 {
    match ops.first() {
        Some((_, MeasurementOp::Open { id, .. })) => *id,
        _ => 0,
    }
}

#[tokio::test(start_paused = true)]
async fn a_frontend_change_stops_the_plan() {
    let r = rig();
    r.measurer.start(forty()).await.expect("plan");
    r.until(NOON + 30_000).await;
    r.measurer.stop_for(StopReason::FrontendChanged);
    r.until(NOON + 6 * MIN).await;
    assert!(
        r.reports().is_empty(),
        "the minute in progress is not reported"
    );
    let stopped: Vec<v1::Event> = r
        .events()
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| e.kind == v1::event::Kind::MeasurementStopped as i32)
        .collect();
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].measurement_id, Some(7));
    assert_eq!(
        stopped[0].measurement_stop_reason,
        Some(v1::MeasurementStopReason::FrontendChanged as i32)
    );
    assert!(r.backend.measurement_ids().is_empty(), "channel closed");
    assert_eq!(r.measurer.current(), None);
    // A new plan may start.
    let mut again = forty();
    again.measurement_id = 8;
    r.measurer.start(again).await.expect("new plan");
    assert_eq!(r.measurer.current(), Some(8));
    r.measurer.shutdown();
    assert_eq!(r.measurer.current(), None);
}

#[tokio::test(start_paused = true)]
async fn stop_and_one_plan_at_a_time() {
    let r = rig();
    r.measurer.start(forty()).await.expect("plan");
    let busy = r.measurer.start(forty()).await.expect_err("second plan");
    assert_eq!(busy.code, v1::ResultCode::AlreadyExists);
    assert!(!r.measurer.stop(9));
    assert!(r.measurer.stop(7));
    assert!(!r.measurer.stop(7));
    r.until(NOON + 2 * MIN).await;
    assert!(r.reports().is_empty());
    assert!(r.backend.measurement_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn refusals() {
    let r = rig_with(false, FakeBackend::default());
    let off = r.measurer.start(forty()).await.expect_err("switched off");
    assert_eq!(off.code, v1::ResultCode::PermissionDenied);
    let r = rig();
    let mut far = forty();
    far.noise_bands[0].ranges = vec![v1::FrequencyRange {
        start_hz: 14_000_000,
        end_hz: 14_350_000,
    }];
    let out = r.measurer.start(far).await.expect_err("not covered");
    assert_eq!(out.code, v1::ResultCode::OutOfRange);
    // The failed start left no plan behind.
    assert_eq!(r.measurer.current(), None);
    for bad in [
        v1::StartMeasurements {
            bin_count: 3,
            ..forty()
        },
        v1::StartMeasurements {
            dwell_ms: 0,
            ..forty()
        },
        v1::StartMeasurements {
            poll_interval_ms: 50,
            ..forty()
        },
        v1::StartMeasurements {
            noise_interval_ms: 90_000,
            ..forty()
        },
        v1::StartMeasurements {
            resolution_bw_hz: f32::NAN,
            ..forty()
        },
    ] {
        let e = r.measurer.start(bad).await.expect_err("invalid");
        assert_eq!(e.code, v1::ResultCode::InvalidArgument, "{}", e.message);
    }
    // A band the hub listed twice or malformed is left out.
    let mut odd = forty();
    odd.noise_bands.push(odd.noise_bands[0].clone());
    odd.noise_bands.push(v1::NoiseBand {
        band_id: 4,
        ranges: vec![],
        key: "BAD KEY".into(),
    });
    let ok = r.measurer.start(odd).await.expect("plan");
    assert_eq!(ok.noise_bands.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failing_backend_stops_the_plan() {
    let backend = FakeBackend::default();
    let r = rig_with(true, backend.clone());
    r.measurer.start(forty()).await.expect("plan");
    backend.set_measurement_failure(true);
    r.until(NOON + MIN).await;
    let stopped: Vec<Option<i32>> = r
        .events()
        .into_iter()
        .filter(|(_, e)| e.kind == v1::event::Kind::MeasurementStopped as i32)
        .map(|(_, e)| e.measurement_stop_reason)
        .collect();
    assert_eq!(
        stopped,
        [Some(v1::MeasurementStopReason::BackendError as i32)]
    );
    assert_eq!(r.measurer.current(), None);
}

#[tokio::test(start_paused = true)]
async fn a_busy_station_pauses_for_five_quiet_minutes() {
    let r = rig();
    let _audio = r.listen(1).await;
    r.measurer.start(forty()).await.expect("plan");
    r.until(NOON + 30_000).await;
    // Busy at 12:01: no report for it, nor until five quiet minutes passed.
    *r.load.lock().expect("load") = 3.5;
    r.until(NOON + MIN + 30_000).await;
    *r.load.lock().expect("load") = 0.2;
    r.until(NOON + 7 * MIN + 1_000).await;
    let starts: Vec<i64> = r
        .reports()
        .iter()
        .map(|n| n.window_start_unix_ms - NOON)
        .collect();
    assert_eq!(starts, [-MIN, 0, 6 * MIN]);
    // The channel was closed while paused.
    let closes = r
        .ops()
        .into_iter()
        .filter(|(_, op)| matches!(op, MeasurementOp::Close { .. }))
        .count();
    assert_eq!(closes, 1);
}

#[tokio::test(start_paused = true)]
async fn missed_polls_make_the_report_partial() {
    let backend = FakeBackend::default();
    let r = rig_with(true, backend.clone());
    let _audio = r.listen(1).await;
    // 11:59:59: too late to open 2 s ahead, but the first spectrum is valid at once.
    r.until(NOON - 1_000).await;
    r.measurer.start(forty()).await.expect("plan");
    r.until(NOON + MIN + 1_000).await;
    let reports = r.reports();
    assert_eq!(reports.len(), 1);
    assert!(!reports[0].partial, "nothing is discarded after the open");
    assert_eq!(reports[0].bands[0].samples, 592 * 120);
    // Polls failing for 10 s of the next minute.
    r.until(NOON + MIN + 10_000).await;
    backend.set_measurement_failure(true);
    r.until(NOON + MIN + 20_000).await;
    backend.set_measurement_failure(false);
    r.until(NOON + 2 * MIN + 1_000).await;
    let reports = r.reports();
    assert_eq!(reports.len(), 2);
    assert!(reports[1].partial);
    assert_eq!(reports[1].window_start_unix_ms, NOON + MIN);
    assert!(reports[1].bands[0].samples < 592 * 120);
    // The capabilities say it measures.
    let caps = dispatch::dispatch(
        &r.requests,
        v1::Request {
            request_id: 50,
            command: Some(v1::request::Command::GetCapabilities(
                v1::GetCapabilities {},
            )),
        },
    )
    .await
    .0;
    let Some(v1::response::Payload::Capabilities(caps)) = caps.payload else {
        panic!("capabilities");
    };
    assert_eq!(
        caps.measurements,
        Some(v1::MeasurementLimits {
            enabled: true,
            max_noise_bands: 32,
            method_version: 1
        })
    );
    assert!(r.backend.can_measure());
}

#[tokio::test(start_paused = true)]
async fn hops_use_their_own_reply_and_stay_on_the_grid() {
    let r = rig();
    let _audio = r.listen(1).await;
    let mut two = forty();
    two.noise_bands.push(v1::NoiseBand {
        band_id: 4,
        ranges: vec![v1::FrequencyRange {
            start_hz: 7_500_000,
            end_hz: 7_900_000,
        }],
        key: "x".into(),
    });
    let started = r.measurer.start(two).await.expect("plan");
    assert_eq!(started.noise_bands.len(), 2);
    r.until(NOON - 30_000).await;
    r.until(NOON + MIN + 1_000).await;
    let reports = r.reports();
    let n = reports
        .iter()
        .find(|n| n.window_start_unix_ms == NOON)
        .expect("12:00");
    assert!(!n.partial);
    // 15 slots alternate: 8 on 40 m, 7 on the other window; every slot is a hop,
    // whose reply is its first spectrum, then 7 polls: 8 spectra a slot.
    let samples: Vec<(u32, u32)> = n.bands.iter().map(|b| (b.band_id, b.samples)).collect();
    assert_eq!(samples, [(3, 592 * 8 * 8), (4, 800 * 7 * 8)]);
    // Every centre is a whole kHz from the front end's centre (the fake's grid).
    for (_, op) in r.ops() {
        if let MeasurementOp::Retune { center_hz, .. } | MeasurementOp::Open { center_hz, .. } = op
        {
            assert_eq!((center_hz - 7_100_000) % 1_000, 0, "{center_hz}");
        }
    }
}
