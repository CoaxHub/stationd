// SPDX-License-Identifier: GPL-3.0-only
//! The plan runner: one task per plan, on the station's wall clock
//! (the measurement design §2.2, §4, §4.1).
//!
//! Minute by minute, at each UTC boundary:
//!
//! 1. Under idle duty with the channel closed, it wakes [`OPEN_BEFORE_MS`]
//!    before a measuring minute and opens the measurement channel, which
//!    starts the front end.
//! 2. At the boundary it decides the duty from the connection's listener
//!    channels and waterfall subscriptions, and sends
//!    `Event{MEASUREMENT_DUTY}` when it changed; it applies the pause rules.
//! 3. A measured minute's 15 dwell slots go round-robin over its windows
//!    (slot `s` → window `s mod n`), [`Shape::dwell_ms`] each, polled every
//!    `poll_interval_ms`. A hop's own reply is the dwell's first spectrum when
//!    the backend's reply carries one (radiod's does); spectra within
//!    [`WARMUP_MS`] of the open or [`HOP_SETTLE_POLLS`] polls of a hop are
//!    discarded (none, from the spike's measurements). A window with fewer
//!    spectra than planned makes the report partial. After the minute it
//!    sends the `NoiseFloorReport`.
//! 4. A minute that is not measured closes the channel [`CLOSE_AFTER_MS`]
//!    after its boundary (the front end stops when the last channel closes).
//!
//! It sends no front-end command: it only opens, retunes, polls and closes
//! its own channel. Reports go to the outbound queue's measurement class
//! (dropped once older than 10 s); duty events are events (never dropped for
//! age). It stops when cancelled (`StopMeasurements`, a frontend change, the
//! end of the connection), and on its own when the backend refuses the
//! channel ([`StopCause`]).

use station_backend::{MeasurementChannel, MeasurementId};
use station_proto::{limits, v1};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::MeasureDeps;
use super::plan::{Plan, Shape, Window};
use super::schedule::{
    Load, MINUTE_MS, PauseGate, RESUME_AFTER_MS, duty_for, is_measured, next_minute,
};
use station_backend::reduce::Histogram;
use v1::station_message::Body;

/// Under idle duty, the channel opens this long before a measuring minute
/// (the measurement design §4.1, `limits::IDLE_OPEN_BEFORE`): radiod's first spectrum is
/// valid about 70 ms after the open (the measurement spike); the rest is margin for a
/// slower start on a Raspberry Pi.
pub const OPEN_BEFORE_MS: i64 = 2_000;

/// After opening the channel, spectra are discarded for this long: none,
/// since radiod answers the first poll only once the front end delivers
/// samples, and that spectrum is valid (the measurement spike).
pub const WARMUP_MS: i64 = 0;

/// Polls discarded after a hop: none, since the hop's own reply already
/// carries the new window (the measurement spike; on radiod's placement grid).
pub const HOP_SETTLE_POLLS: u32 = 0;

/// A minute that is not measured closes the channel this long after its
/// boundary (the last poll's guard).
pub const CLOSE_AFTER_MS: i64 = 500;

/// Measured minutes in a row whose every poll failed before the plan stops.
pub const MAX_FAILED_MINUTES: u32 = 3;

/// Why a plan stopped on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopCause {
    /// The backend refused or lost the channel.
    BackendError,
}

enum Halt {
    Cancelled,
    Stopped(StopCause),
}

/// What a measured minute pooled.
struct Minute {
    hists: Vec<Option<Histogram>>,
    planned: Vec<u32>,
    pooled: Vec<u32>,
    polls_ok: u32,
}

pub(crate) struct Runner {
    pub measurement_id: u32,
    pub channel: MeasurementId,
    pub plan: Plan,
    pub shape: Shape,
    pub poll_interval_ms: u32,
    pub deps: MeasureDeps,
    pub stop: CancellationToken,
    /// The duty announced in `MeasurementsStarted`.
    pub duty: v1::MeasurementDuty,
    /// The plan's front end, for reports when the backend can't say.
    pub frontend: v1::FrontendState,
}

struct State {
    open: bool,
    warm_until_ms: i64,
    center_hz: Option<u64>,
    measured: u64,
    paused: bool,
    failed_minutes: u32,
    pause: PauseGate,
}

impl Runner {
    /// Runs until cancelled (`None`) or stopped by the backend.
    pub async fn run(self) -> Option<StopCause> {
        let mut st = State {
            open: false,
            warm_until_ms: 0,
            center_hz: None,
            measured: 0,
            paused: false,
            failed_minutes: 0,
            pause: PauseGate::default(),
        };
        let mut duty = self.duty;
        let outcome = self.run_inner(&mut st, &mut duty).await;
        if st.open {
            if let Err(e) = self.deps.backend.close_measurement(self.channel).await {
                debug!(error = %e, "closing the measurement channel");
            }
        }
        match outcome {
            Halt::Cancelled => None,
            Halt::Stopped(cause) => Some(cause),
        }
    }

    async fn wait(&self, unix_ms: i64) -> Result<(), Halt> {
        tokio::select! {
            biased;
            () = self.stop.cancelled() => Err(Halt::Cancelled),
            () = self.deps.clock.sleep_until(unix_ms) => Ok(()),
        }
    }

    fn now(&self) -> i64 {
        self.deps.clock.now_ms()
    }

    fn open_count(&self) -> usize {
        let t = self
            .deps
            .tables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        t.open_channels() + t.open_subscriptions()
    }

    fn load(&self) -> Load {
        let host = (self.deps.host)();
        let health = self.deps.backend.health();
        Load {
            load_avg_1m: host.load_avg_1m,
            cpu_count: host.cpu_count,
            dropped_total: self.deps.drops.audio() + self.deps.drops.waterfall(),
            backend_degraded: health.backend.state == v1::backend_health::State::Degraded as i32,
        }
    }

    async fn open_channel(&self, st: &mut State, center_hz: u64) -> Result<(), Halt> {
        let ch = MeasurementChannel {
            center_hz,
            bin_count: self.shape.bin_count,
            resolution_bw_hz: self.shape.resolution_bw_hz,
            poll_interval: std::time::Duration::from_millis(u64::from(self.poll_interval_ms)),
        };
        if let Err(e) = self.deps.backend.open_measurement(self.channel, ch).await {
            warn!(error = %e, "the backend refused the measurement channel; measuring stops");
            return Err(Halt::Stopped(StopCause::BackendError));
        }
        st.open = true;
        st.center_hz = Some(center_hz);
        st.warm_until_ms = self.now() + WARMUP_MS;
        debug!(center_hz, "measurement channel open");
        Ok(())
    }

    async fn close_channel(&self, st: &mut State) {
        if !st.open {
            return;
        }
        st.open = false;
        st.center_hz = None;
        if let Err(e) = self.deps.backend.close_measurement(self.channel).await {
            debug!(error = %e, "closing the measurement channel");
        }
        debug!("measurement channel closed");
    }

    /// The windows of the `n`-th measured minute of the plan.
    fn windows(&self, n: u64) -> (&[Window], u32) {
        let cycle = u64::from(self.plan.cycle_minutes().max(1));
        let index = u32::try_from(n % cycle).unwrap_or(0);
        let windows = self
            .plan
            .minutes
            .get(index as usize)
            .map_or(&[][..], Vec::as_slice);
        (windows, index)
    }

    fn first_center(&self, n: u64) -> Option<u64> {
        self.windows(n).0.first().map(|w| w.center_hz)
    }

    async fn run_inner(&self, st: &mut State, duty: &mut v1::MeasurementDuty) -> Halt {
        let mut minute = next_minute(self.now());
        loop {
            match self.one_minute(st, duty, minute).await {
                Ok(()) => {}
                Err(halt) => return halt,
            }
            minute += MINUTE_MS;
        }
    }

    async fn one_minute(
        &self,
        st: &mut State,
        duty: &mut v1::MeasurementDuty,
        minute: i64,
    ) -> Result<(), Halt> {
        // 1. Warm the front end up for a minute that will be measured.
        if !st.open {
            self.wait(minute - OPEN_BEFORE_MS).await?;
            let paused = st.pause.is_paused(self.now());
            let predicted = duty_for(self.open_count());
            let center = self.first_center(st.measured);
            if let Some(center) = center.filter(|_| is_measured(predicted, minute) && !paused) {
                self.open_channel(st, center).await?;
            }
        }
        // 2. The boundary: duty and pause.
        self.wait(minute).await?;
        let now_duty = duty_for(self.open_count());
        if now_duty != *duty {
            *duty = now_duty;
            self.send_duty(now_duty, minute);
        }
        let paused = st.pause.observe(self.now(), self.load());
        if paused != st.paused {
            st.paused = paused;
            if paused {
                info!(
                    "measuring paused: the station is busy (load, dropped media or a degraded backend)"
                );
            } else {
                info!(
                    quiet_s = RESUME_AFTER_MS / 1000,
                    "measuring resumed after a quiet spell"
                );
            }
        }
        let (windows, cycle_index) = self.windows(st.measured);
        let scheduled = is_measured(*duty, minute) && !paused;
        if scheduled && windows.is_empty() {
            // An empty minute of a rounded-up cycle still counts.
            st.measured += 1;
        }
        if !scheduled || windows.is_empty() {
            if st.open {
                self.wait(minute + CLOSE_AFTER_MS).await?;
                self.close_channel(st).await;
            }
            return Ok(());
        }
        // 3. Measure it.
        if !st.open {
            let center = windows.first().map_or(0, |w| w.center_hz);
            self.open_channel(st, center).await?;
        }
        st.measured += 1;
        let windows = windows.to_vec();
        let overloads_before = self.overloads();
        let pooled = self.measure(st, minute, &windows).await?;
        self.wait(minute + MINUTE_MS).await?;
        if pooled.polls_ok == 0 {
            st.failed_minutes += 1;
            if st.failed_minutes >= MAX_FAILED_MINUTES {
                warn!("every poll failed for {MAX_FAILED_MINUTES} minutes; measuring stops");
                return Err(Halt::Stopped(StopCause::BackendError));
            }
        } else {
            st.failed_minutes = 0;
        }
        let overloads = self.overloads().saturating_sub(overloads_before);
        self.send_report(minute, cycle_index, &pooled, overloads)
            .await;
        Ok(())
    }

    fn overloads(&self) -> u64 {
        self.deps
            .backend
            .health()
            .frontend
            .map_or(0, |f| f.overloads_total)
    }

    async fn measure(
        &self,
        st: &mut State,
        minute: i64,
        windows: &[Window],
    ) -> Result<Minute, Halt> {
        let bands = self.plan.bands.len();
        let mut m = Minute {
            hists: vec![None; bands],
            planned: vec![0; bands],
            pooled: vec![0; bands],
            polls_ok: 0,
        };
        let dwell = i64::from(self.shape.dwell_ms);
        let poll = i64::from(self.poll_interval_ms.max(1));
        let polls = u32::try_from((dwell / poll).max(1)).unwrap_or(1);
        for slot in 0..self.plan.slots_per_minute {
            let Some(w) = windows.get(slot as usize % windows.len().max(1)) else {
                break;
            };
            let t0 = minute + i64::from(slot) * dwell;
            self.wait(t0).await?;
            let mut settle = 0;
            let mut hop_reply = None;
            if st.center_hz != Some(w.center_hz) {
                match self
                    .deps
                    .backend
                    .retune_measurement(self.channel, w.center_hz)
                    .await
                {
                    Ok(reply) => hop_reply = reply,
                    Err(e) if e.code == v1::ResultCode::NotFound => {
                        warn!("the measurement channel is gone; measuring stops");
                        return Err(Halt::Stopped(StopCause::BackendError));
                    }
                    Err(e) => {
                        debug!(error = %e, "retuning the measurement channel");
                        for &b in &w.bands {
                            m.planned[b] += polls.saturating_sub(HOP_SETTLE_POLLS);
                        }
                        continue;
                    }
                }
                st.center_hz = Some(w.center_hz);
                settle = HOP_SETTLE_POLLS;
            }
            for j in 0..polls {
                // The hop's own reply stands for the dwell's first poll.
                let reply = if j == 0 && settle == 0 {
                    hop_reply.take()
                } else {
                    None
                };
                if reply.is_none() {
                    self.wait(t0 + poll / 2 + i64::from(j) * poll).await?;
                }
                if j < settle {
                    continue;
                }
                for &b in &w.bands {
                    m.planned[b] += 1;
                }
                if self.now() < st.warm_until_ms {
                    continue;
                }
                let polled = match reply {
                    Some(s) => Ok(s),
                    None => self.deps.backend.poll_measurement(self.channel).await,
                };
                match polled {
                    Ok(spectrum) => {
                        m.polls_ok += 1;
                        for &b in &w.bands {
                            let Some(band) = self.plan.bands.get(b) else {
                                continue;
                            };
                            let hist = m.hists[b].get_or_insert_with(Histogram::default);
                            hist.add_spectrum(&spectrum, &band.ranges);
                            m.pooled[b] += 1;
                        }
                    }
                    Err(e) => debug!(error = %e, "polling the measurement channel"),
                }
            }
        }
        Ok(m)
    }

    fn send_duty(&self, duty: v1::MeasurementDuty, minute: i64) {
        debug!(duty = duty.as_str_name(), minute, "duty changes");
        self.deps.outbound.push_body(Body::Event(v1::Event {
            kind: v1::event::Kind::MeasurementDuty as i32,
            measurement_id: Some(self.measurement_id),
            measurement_duty: Some(duty as i32),
            effective_unix_ms: Some(minute),
            ..Default::default()
        }));
    }

    async fn send_report(&self, minute: i64, cycle_index: u32, m: &Minute, overloads: u64) {
        let mut partial = false;
        let mut bands = Vec::new();
        for (i, band) in self.plan.bands.iter().enumerate() {
            if m.planned[i] == 0 {
                continue;
            }
            if m.pooled[i] < m.planned[i] {
                partial = true;
            }
            let Some(hist) = &m.hists[i] else {
                continue;
            };
            let (Some(q), Some(first), Some(last)) =
                (hist.quantiles(), band.ranges.first(), band.ranges.last())
            else {
                continue;
            };
            let Some((lo, hi)) = hist.measured((first.0, last.1)) else {
                continue;
            };
            bands.push(v1::BandNoise {
                band_id: band.band_id,
                measured: Some(v1::FrequencyRange {
                    start_hz: lo,
                    end_hz: hi,
                }),
                resolution_bw_hz: self.shape.resolution_bw_hz,
                samples: u32::try_from(hist.count())
                    .unwrap_or(u32::MAX)
                    .min(limits::MAX_NOISE_SAMPLES),
                p10_db_hz: q.p10,
                p25_db_hz: q.p25,
                p50_db_hz: q.p50,
                p90_db_hz: q.p90,
            });
        }
        if bands.is_empty() {
            debug!(minute, "nothing measured this minute; no report");
            return;
        }
        let frontend = match self.deps.backend.frontend_state().await {
            Ok(f) => f,
            Err(_) => self.frontend.clone(),
        };
        let report = v1::NoiseFloorReport {
            measurement_id: self.measurement_id,
            window_start_unix_ms: minute,
            window_ms: limits::NOISE_WINDOW_MS,
            frontend: Some(sanitize_frontend(frontend)),
            overloads: u32::try_from(overloads).unwrap_or(u32::MAX),
            clock: Some((self.deps.clock_state)()),
            bands,
            partial,
            cycle_minutes: self.plan.cycle_minutes(),
            cycle_index,
        };
        self.deps.outbound.push_body(Body::Noise(report));
    }
}

/// A `FrontendState` the hub's checks pass: finite floats, a short port.
pub(crate) fn sanitize_frontend(mut f: v1::FrontendState) -> v1::FrontendState {
    if !f.gain_db.is_finite() {
        f.gain_db = 0.0;
    }
    if !f.attenuation_db.is_finite() {
        f.attenuation_db = 0.0;
    }
    f.antenna_port =
        crate::dispatch::truncate_utf8(&f.antenna_port, limits::MAX_ANTENNA_PORT_BYTES);
    if let Some(r) = f.tunable_range.as_mut() {
        r.end_hz = r.end_hz.min(limits::MAX_FREQUENCY_HZ);
        r.start_hz = r.start_hz.min(r.end_hz);
    }
    f.center_frequency_hz = f.center_frequency_hz.min(limits::MAX_FREQUENCY_HZ);
    f
}
