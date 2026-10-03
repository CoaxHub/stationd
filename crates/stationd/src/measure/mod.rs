// SPDX-License-Identifier: GPL-3.0-only
//! Measuring the noise floor (station protocol 1.1).
//!
//! The hub asks with a plan (`StartMeasurements`); the station runs it on
//! its own clock, on a spectrum channel of its own that is not listener
//! capacity, and never retunes the front end for it:
//!
//! - [`plan`]: which windows the measurement channel visits, and when;
//! - `station_backend::reduce`: spectra to the quantiles of a band's minute
//!   (shared with the measurement spike);
//! - [`schedule`]: the duty (continuous while listened to, one minute in five
//!   while idle) and the pause rules;
//! - [`clock`]: the wall clock the schedule follows, and the clock state;
//! - the runner (one task per plan), and [`Measurer`]: one per connection,
//!   which answers `StartMeasurements`/`StopMeasurements` and stops the plan
//!   when the front end changes (`Event{MEASUREMENT_STOPPED}`) or the
//!   connection ends (silently: a plan belongs to one connection, and
//!   nothing is buffered across a disconnect).
//!
//! `[measurements] enabled = false` in `stationd.toml` turns it off on the
//! station: `Capabilities.measurements.enabled` is false and every plan is
//! refused with `PERMISSION_DENIED`.

pub mod clock;
pub mod plan;
mod runner;
pub mod schedule;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use station_backend::Backend;
use station_proto::{limits, v1};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::dispatch::Tables;
use crate::outbound::{DropCounters, Outbound};

pub use self::clock::{MeasureClock, ScaledClock, SystemClock};
pub use self::runner::{CLOSE_AFTER_MS, HOP_SETTLE_POLLS, OPEN_BEFORE_MS, WARMUP_MS};
use self::runner::{Runner, StopCause};
use self::schedule::duty_for;

/// What the measurement code needs from the connection.
#[derive(Clone)]
pub struct MeasureDeps {
    /// The DSP backend.
    pub backend: Arc<dyn Backend>,
    /// Where reports and events go.
    pub outbound: Arc<Outbound>,
    /// The connection's channels and subscriptions (the duty).
    pub tables: Arc<Mutex<Tables>>,
    /// The queue's drop counters (the pause rules).
    pub drops: Arc<DropCounters>,
    /// The schedule's wall clock.
    pub clock: Arc<dyn MeasureClock>,
    /// Host metrics (the load average).
    pub host: Arc<dyn Fn() -> v1::HostHealth + Send + Sync>,
    /// The clock state reports carry.
    pub clock_state: Arc<dyn Fn() -> v1::ClockState + Send + Sync>,
    /// `[measurements] enabled`.
    pub enabled: bool,
}

impl std::fmt::Debug for MeasureDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeasureDeps")
            .field("clock", &self.clock)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// The `Capabilities.measurements` of a station whose backend `can_measure`,
/// with the owner's switch.
pub fn limits_for(can_measure: bool, enabled: bool) -> Option<v1::MeasurementLimits> {
    can_measure.then_some(v1::MeasurementLimits {
        enabled,
        max_noise_bands: limits::MAX_NOISE_BANDS,
        method_version: limits::MEASURE_METHOD_VERSION,
    })
}

/// A refused `StartMeasurements`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The result code.
    pub code: v1::ResultCode,
    /// For the owner's dashboard.
    pub message: String,
}

fn refuse(code: v1::ResultCode, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
    }
}

/// Why a plan stops other than by `StopMeasurements`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `SetFrontend`, or `Event{FRONTEND_CHANGED}`.
    FrontendChanged,
    /// `Event{BACKEND_RESTARTED}`.
    BackendRestarted,
}

impl StopReason {
    fn proto(self) -> v1::MeasurementStopReason {
        match self {
            Self::FrontendChanged => v1::MeasurementStopReason::FrontendChanged,
            Self::BackendRestarted => v1::MeasurementStopReason::BackendRestarted,
        }
    }
}

#[derive(Debug)]
struct Running {
    measurement_id: u32,
    stop: CancellationToken,
}

/// The plan of one connection (at most one).
#[derive(Debug)]
pub struct Measurer {
    deps: MeasureDeps,
    running: Mutex<Option<Running>>,
    next_channel: AtomicU32,
}

/// Channel ids are `stationd`'s own; one plan at a time needs few.
static CHANNEL_IDS: AtomicU32 = AtomicU32::new(1);

impl Measurer {
    /// No plan yet.
    pub fn new(deps: MeasureDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            running: Mutex::new(None),
            next_channel: AtomicU32::new(CHANNEL_IDS.fetch_add(1 << 16, Ordering::Relaxed)),
        })
    }

    fn running(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The id of the running plan.
    pub fn current(&self) -> Option<u32> {
        self.running().as_ref().map(|r| r.measurement_id)
    }

    /// Answers `StartMeasurements`: the plan as it will run, or why not.
    pub async fn start(
        self: &Arc<Self>,
        req: v1::StartMeasurements,
    ) -> Result<v1::MeasurementsStarted, Refusal> {
        if !self.deps.enabled {
            return Err(refuse(
                v1::ResultCode::PermissionDenied,
                "measuring is turned off in stationd.toml ([measurements] enabled = false)",
            ));
        }
        if !self.deps.backend.can_measure() {
            return Err(refuse(
                v1::ResultCode::Unsupported,
                "this backend cannot measure",
            ));
        }
        let shape = check_shape(&req)?;
        let stop = CancellationToken::new();
        {
            let mut running = self.running();
            if running.is_some() {
                return Err(refuse(
                    v1::ResultCode::AlreadyExists,
                    "a plan is already running on this connection",
                ));
            }
            *running = Some(Running {
                measurement_id: req.measurement_id,
                stop: stop.clone(),
            });
        }
        match self.prepare(&req, shape).await {
            Ok((started, runner)) => {
                self.spawn(runner, req.measurement_id, stop);
                info!(
                    measurement_id = req.measurement_id,
                    bands = started.noise_bands.len(),
                    duty = v1::MeasurementDuty::try_from(started.duty)
                        .map_or("unknown", |d| d.as_str_name()),
                    "measuring the noise floor"
                );
                Ok(started)
            }
            Err(refusal) => {
                self.clear(req.measurement_id);
                Err(refusal)
            }
        }
    }

    async fn prepare(
        &self,
        req: &v1::StartMeasurements,
        shape: plan::Shape,
    ) -> Result<(v1::MeasurementsStarted, Runner), Refusal> {
        let backend_error = |e: station_backend::BackendError| refuse(e.code, e.message);
        let caps = self
            .deps
            .backend
            .capabilities()
            .await
            .map_err(backend_error)?;
        let frontend = runner::sanitize_frontend(
            self.deps
                .backend
                .frontend_state()
                .await
                .map_err(backend_error)?,
        );
        let tunable = caps
            .tunable_range
            .or(frontend.tunable_range)
            .ok_or_else(|| refuse(v1::ResultCode::BackendError, "no tunable range"))?;
        let fe = plan::FrontEnd {
            tunable: (tunable.start_hz, tunable.end_hz),
            center_hz: frontend.center_frequency_hz,
        };
        let bands: Vec<plan::BandRequest> = req
            .noise_bands
            .iter()
            .filter(|b| b.band_id != 0 && station_proto::validate::is_band_key(&b.key))
            .filter_map(|b| {
                let ranges: Vec<(u64, u64)> = b
                    .ranges
                    .iter()
                    .take(limits::MAX_BAND_RANGES)
                    .map(|r| (r.start_hz, r.end_hz.min(limits::MAX_FREQUENCY_HZ)))
                    .collect();
                let sorted =
                    ranges.iter().all(|(s, e)| s < e) && ranges.windows(2).all(|w| w[0].1 < w[1].0);
                (sorted && !ranges.is_empty()).then(|| (b.band_id, b.key.clone(), ranges))
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        let bands: Vec<_> = bands.into_iter().filter(|b| seen.insert(b.0)).collect();
        let grid = self
            .deps
            .backend
            .measurement_grid(shape.resolution_bw_hz)
            .await;
        let plan = plan::plan(&bands, fe, shape, grid, limits::MAX_NOISE_BANDS as usize);
        if plan.bands.is_empty() {
            return Err(refuse(
                v1::ResultCode::OutOfRange,
                "no band of the plan is inside the tunable range",
            ));
        }
        let cycle_ms = plan.cycle_minutes() * limits::NOISE_WINDOW_MS;
        let interval = limits::NOISE_INTERVALS_MS
            .iter()
            .copied()
            .find(|&i| i >= cycle_ms.max(req.noise_interval_ms))
            .unwrap_or(cycle_ms);
        let duty = duty_for({
            let t = self
                .deps
                .tables
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            t.open_channels() + t.open_subscriptions()
        });
        let started = v1::MeasurementsStarted {
            measurement_id: req.measurement_id,
            noise_bands: plan
                .bands
                .iter()
                .map(|b| v1::NoiseBand {
                    band_id: b.band_id,
                    ranges: b
                        .ranges
                        .iter()
                        .map(|&(start_hz, end_hz)| v1::FrequencyRange { start_hz, end_hz })
                        .collect(),
                    key: b.key.clone(),
                })
                .collect(),
            noise_interval_ms: interval,
            duty: duty as i32,
            frontend: Some(frontend.clone()),
            method_version: limits::MEASURE_METHOD_VERSION,
        };
        let runner = Runner {
            measurement_id: req.measurement_id,
            channel: self.next_channel.fetch_add(1, Ordering::Relaxed),
            plan,
            shape,
            poll_interval_ms: req.poll_interval_ms,
            deps: self.deps.clone(),
            stop: CancellationToken::new(),
            duty,
            frontend,
        };
        Ok((started, runner))
    }

    fn spawn(self: &Arc<Self>, mut runner: Runner, measurement_id: u32, stop: CancellationToken) {
        runner.stop = stop;
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let cause = runner.run().await;
            if let (Some(StopCause::BackendError), Some(me)) = (cause, me.upgrade()) {
                if me.clear(measurement_id) {
                    me.send_stopped(measurement_id, v1::MeasurementStopReason::BackendError);
                }
            }
        });
    }

    /// Forgets plan `id` if it is the running one; returns whether it was.
    fn clear(&self, id: u32) -> bool {
        let mut running = self.running();
        if running.as_ref().is_some_and(|r| r.measurement_id == id) {
            if let Some(r) = running.take() {
                r.stop.cancel();
            }
            true
        } else {
            false
        }
    }

    /// Answers `StopMeasurements`: whether plan `id` was running.
    pub fn stop(&self, id: u32) -> bool {
        let stopped = self.clear(id);
        if stopped {
            info!(measurement_id = id, "measuring stopped by the hub");
        }
        stopped
    }

    /// Stops the running plan on the station's own account and tells the hub
    /// (`Event{MEASUREMENT_STOPPED}`): a measurement never straddles a change
    /// of settings or coverage.
    pub fn stop_for(&self, reason: StopReason) {
        let Some(id) = self.current() else {
            return;
        };
        if self.clear(id) {
            warn!(measurement_id = id, ?reason, "measuring stopped");
            self.send_stopped(id, reason.proto());
        }
    }

    fn send_stopped(&self, id: u32, reason: v1::MeasurementStopReason) {
        self.deps
            .outbound
            .push_body(v1::station_message::Body::Event(v1::Event {
                kind: v1::event::Kind::MeasurementStopped as i32,
                measurement_id: Some(id),
                measurement_stop_reason: Some(reason as i32),
                ..Default::default()
            }));
    }

    /// The connection ended: stop without a word (§2.3).
    pub fn shutdown(&self) {
        if let Some(r) = self.running().take() {
            r.stop.cancel();
        }
    }
}

/// The measurement channel's shape from the request, within the station's
/// bounds.
fn check_shape(req: &v1::StartMeasurements) -> Result<plan::Shape, Refusal> {
    let invalid = |m: &str| refuse(v1::ResultCode::InvalidArgument, m);
    if !limits::MEASURE_BIN_COUNTS.contains(&req.bin_count) {
        return Err(invalid("bin_count out of bounds"));
    }
    if !limits::is_measurement_rbw(req.resolution_bw_hz) {
        return Err(invalid("resolution_bw_hz out of bounds"));
    }
    if !limits::MEASURE_DWELL_MS_RANGE.contains(&req.dwell_ms) {
        return Err(invalid("dwell_ms out of bounds"));
    }
    if !limits::MEASURE_POLL_INTERVAL_MS_RANGE.contains(&req.poll_interval_ms)
        || req.poll_interval_ms > req.dwell_ms
    {
        return Err(invalid("poll_interval_ms out of bounds"));
    }
    if !limits::NOISE_INTERVALS_MS.contains(&req.noise_interval_ms) {
        return Err(invalid("noise_interval_ms must be 60000, 120000 or 300000"));
    }
    Ok(plan::Shape {
        bin_count: req.bin_count,
        resolution_bw_hz: req.resolution_bw_hz,
        dwell_ms: req.dwell_ms,
    })
}
