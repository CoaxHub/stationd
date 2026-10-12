// SPDX-License-Identifier: GPL-3.0-only
//! Measuring the noise floor (station protocol §4.1).
//!
//! The hub asks with a plan (`StartMeasurements`) whose bands each lie in
//! one receiver; the station runs it on its own clock, on a spectrum channel
//! of its own per receiver measured (not listener capacity), and never
//! retunes a front end for it:
//!
//! - [`plan`]: which windows a receiver's measurement channel visits, and
//!   when;
//! - `station_backend::reduce`: spectra to the quantiles of a band's minute;
//! - [`schedule`]: the duty (continuous while a receiver is listened to, one
//!   minute in five while idle) and the pause rules;
//! - [`clock`]: the wall clock the schedule follows, and the clock state;
//! - the runner (one task per receiver of the plan), and [`Measurer`]: one
//!   per connection, which answers `StartMeasurements`/`StopMeasurements`
//!   and stops the whole plan when a front end or a receiver changes
//!   (`Event{MEASUREMENT_STOPPED}`) or the connection ends (silently: a plan
//!   belongs to one connection, and nothing is buffered across a
//!   disconnect).
//!
//! Whether a station measures is the owner's choice, kept by the control
//! plane: the hub sends no plan to a station whose measuring is off.

pub mod clock;
pub mod load;
pub mod plan;
mod runner;
pub mod schedule;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use station_proto::{limits, v1};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::dispatch::Tables;
use crate::outbound::{DropCounters, Outbound};
use crate::receivers::Receivers;

pub use self::clock::{MeasureClock, ScaledClock, SystemClock};
pub use self::load::{FixedLoad, HostLoad, LoadSource, ProcLoad};
pub use self::runner::{CLOSE_AFTER_MS, HOP_SETTLE_POLLS, OPEN_BEFORE_MS, WARMUP_MS};
use self::runner::{Runner, StopCause};
use self::schedule::duty_for;

/// What the measurement code needs from the connection.
#[derive(Clone)]
pub struct MeasureDeps {
    /// The station's receivers.
    pub receivers: Receivers,
    /// Where reports and events go.
    pub outbound: Arc<Outbound>,
    /// The connection's channels and subscriptions (each receiver's duty).
    pub tables: Arc<Mutex<Tables>>,
    /// The queue's drop counters (the pause rules).
    pub drops: Arc<DropCounters>,
    /// The schedule's wall clock.
    pub clock: Arc<dyn MeasureClock>,
    /// The host's load (the pause rule).
    pub load: Arc<dyn LoadSource>,
    /// The clock state reports carry.
    pub clock_state: Arc<dyn Fn() -> v1::ClockState + Send + Sync>,
}

impl std::fmt::Debug for MeasureDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeasureDeps")
            .field("clock", &self.clock)
            .field("load", &self.load)
            .finish_non_exhaustive()
    }
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
    /// `SetFrontend`, `Event{FRONTEND_CHANGED}`, or a receiver entering or
    /// leaving `RUNNING` (`Event{RECEIVER_STATE}`).
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

/// A requested band, checked: (`band_id`, `receiver_id`, key, ranges).
type Requested = (u32, u32, String, Vec<(u64, u64)>);

/// The well-formed bands of `req`, each once: a band ID not 0, a key of
/// `[a-z0-9]{1,8}`, 1–8 ranges sorted and apart.
fn requested_bands(req: &v1::StartMeasurements) -> Vec<Requested> {
    let mut seen = std::collections::HashSet::new();
    req.noise_bands
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
            (sorted && !ranges.is_empty())
                .then(|| (b.band_id, b.receiver_id, b.key.clone(), ranges))
        })
        .filter(|b| seen.insert(b.0))
        .collect()
}

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
        if !self.deps.receivers.can_measure() {
            return Err(refuse(
                v1::ResultCode::Unsupported,
                "this station's backend cannot measure",
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
            Ok((started, runners)) => {
                for runner in runners {
                    self.spawn(runner, req.measurement_id, stop.clone());
                }
                info!(
                    measurement_id = req.measurement_id,
                    bands = started.noise_bands.len(),
                    receivers = ?started.receivers.iter().map(|r| r.receiver_id).collect::<Vec<_>>(),
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

    /// Plans each receiver of `req` that runs: its accepted bands, its
    /// `MeasuredReceiver` and its runner.
    async fn prepare(
        &self,
        req: &v1::StartMeasurements,
        shape: plan::Shape,
    ) -> Result<(v1::MeasurementsStarted, Vec<Runner>), Refusal> {
        let bands = requested_bands(req);
        let mut order: Vec<u32> = Vec::new();
        for b in &bands {
            if !order.contains(&b.1) {
                order.push(b.1);
            }
        }
        let mut budget = limits::MAX_NOISE_BANDS as usize;
        let mut accepted = Vec::new();
        let mut measured = Vec::new();
        let mut runners = Vec::new();
        for receiver_id in order {
            let mine: Vec<plan::BandRequest> = bands
                .iter()
                .filter(|b| b.1 == receiver_id)
                .map(|(id, _, key, ranges)| (*id, key.clone(), ranges.clone()))
                .collect();
            let Some((runner, interval)) = self
                .plan_receiver(receiver_id, &mine, req, shape, budget)
                .await
            else {
                continue;
            };
            budget = budget.saturating_sub(runner.plan.bands.len());
            accepted.extend(runner.plan.bands.iter().map(|b| {
                v1::NoiseBand {
                    band_id: b.band_id,
                    receiver_id,
                    ranges: b
                        .ranges
                        .iter()
                        .map(|&(start_hz, end_hz)| v1::FrequencyRange { start_hz, end_hz })
                        .collect(),
                    key: b.key.clone(),
                }
            }));
            measured.push(v1::MeasuredReceiver {
                receiver_id,
                noise_interval_ms: interval,
                duty: runner.duty as i32,
                frontend: Some(runner.frontend.clone()),
            });
            runners.push(runner);
        }
        if accepted.is_empty() {
            return Err(refuse(
                v1::ResultCode::OutOfRange,
                "no band of the plan is inside a running receiver's tunable range",
            ));
        }
        let started = v1::MeasurementsStarted {
            measurement_id: req.measurement_id,
            noise_bands: accepted,
            receivers: measured,
            method_version: limits::MEASURE_METHOD_VERSION,
        };
        Ok((started, runners))
    }

    /// The runner of `bands` (at most `budget` of them) on receiver
    /// `receiver_id`, with its cadence; `None` when it doesn't run or no
    /// band is inside its tunable range.
    async fn plan_receiver(
        &self,
        receiver_id: u32,
        bands: &[plan::BandRequest],
        req: &v1::StartMeasurements,
        shape: plan::Shape,
        budget: usize,
    ) -> Option<(Runner, u32)> {
        let backend = self.deps.receivers.running_backend(receiver_id)?;
        let caps = backend.capabilities().await.ok()?;
        let mut frontend = runner::sanitize_frontend(backend.frontend_state().await.ok()?);
        frontend.receiver_id = receiver_id;
        let tunable = caps.receiver.tunable_range.or(frontend.tunable_range)?;
        let fe = plan::FrontEnd {
            tunable: (tunable.start_hz, tunable.end_hz),
            center_hz: frontend.center_frequency_hz,
        };
        let grid = backend.measurement_grid(shape.resolution_bw_hz).await;
        let plan = plan::plan(bands, fe, shape, grid, budget);
        if plan.bands.is_empty() {
            return None;
        }
        let cycle_ms = plan.cycle_minutes() * limits::NOISE_WINDOW_MS;
        let interval = limits::NOISE_INTERVALS_MS
            .iter()
            .copied()
            .find(|&i| i >= cycle_ms.max(req.noise_interval_ms))
            .unwrap_or(cycle_ms);
        let duty = duty_for(
            self.deps
                .tables
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .listeners_on(receiver_id),
        );
        let runner = Runner {
            measurement_id: req.measurement_id,
            receiver_id,
            backend,
            channel: self.next_channel.fetch_add(1, Ordering::Relaxed),
            plan,
            shape,
            poll_interval_ms: req.poll_interval_ms,
            deps: self.deps.clone(),
            stop: CancellationToken::new(),
            duty,
            frontend,
        };
        Some((runner, interval))
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

    /// Forgets plan `id` if it is the running one, stopping every receiver
    /// of it; returns whether it was.
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
