// SPDX-License-Identifier: MIT
//! §5.2: measurements (protocol 1.1, measure.proto).
//!
//! - **Stateless** ([`check_noise_report`], [`check_measurements_started`]):
//!   sizes, finite floats, quantiles in −250…50 dB/Hz and in order, ranges
//!   sorted and apart, known enums, `frontend` present. Violations.
//! - **Stateful** ([`Plans`], driven by [`super::Session`]): a plan opens when
//!   the hub sends `StartMeasurements` and closes (with the 5 s grace of
//!   channels) on `StopMeasurements` or `Event{MEASUREMENT_STOPPED}`.
//!   `MeasurementsStarted` must echo the request (ids and ranges ⊆ the
//!   requested ones, cadence ≥ the requested one); a report may name only
//!   accepted bands, once each, with `measured` inside the band; reports and
//!   duty changes are rate-limited (drop first, a violation when sustained).
//! - **Plausibility** (dropped and counted, never a strike: an honest station
//!   can produce it): with the hub's wall clock, a window that does not start
//!   on a UTC minute or lies outside [now − 15 min, now + 5 s], and a duty
//!   change outside [now − 60 s, now + 65 s]; and under idle duty, a window
//!   that is not a measuring minute.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::fields::{bounds, check_enum, check_finite, check_frontend_state, check_len, missing};
use super::rate::{Sustained, TokenBucket};
use super::{DropReason, MessageKind, ViolationKind};
use crate::limits;
use crate::v1;

/// Whether `key` is a band key: 1–[`limits::MAX_BAND_KEY_BYTES`] bytes of
/// `[a-z0-9]`.
pub fn is_band_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= limits::MAX_BAND_KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// §4.1: whether the UTC minute containing `unix_ms` is a measuring minute
/// (its number is a multiple of [`limits::IDLE_PERIOD_MINUTES`]: hh:00, hh:05, …).
pub fn is_measuring_minute(unix_ms: i64) -> bool {
    unix_ms
        .div_euclid(60_000)
        .rem_euclid(limits::IDLE_PERIOD_MINUTES)
        == 0
}

/// The duty an `i32` names, if it is continuous or idle.
pub fn duty_of(raw: i32) -> Option<v1::MeasurementDuty> {
    match v1::MeasurementDuty::try_from(raw) {
        Ok(d @ (v1::MeasurementDuty::Continuous | v1::MeasurementDuty::Idle)) => Some(d),
        _ => None,
    }
}

// ------------------------------------------------------------------ stateless

/// Ranges of one band: 1–[`limits::MAX_BAND_RANGES`], `start < end ≤ 6 GHz`,
/// sorted and apart.
fn check_ranges(field: &'static str, ranges: &[v1::FrequencyRange]) -> Result<(), ViolationKind> {
    if ranges.is_empty() {
        return Err(missing(field));
    }
    if ranges.len() > limits::MAX_BAND_RANGES {
        return Err(ViolationKind::TooManyEntries {
            field,
            len: ranges.len(),
            max: limits::MAX_BAND_RANGES,
        });
    }
    let ordered = ranges
        .iter()
        .all(|r| r.start_hz < r.end_hz && r.end_hz <= limits::MAX_FREQUENCY_HZ)
        && ranges.windows(2).all(|w| w[0].end_hz < w[1].start_hz);
    if ordered {
        Ok(())
    } else {
        Err(ViolationKind::InvalidRange { field })
    }
}

fn check_band_list(field: &'static str, bands: &[v1::NoiseBand]) -> Result<(), ViolationKind> {
    let max = limits::MAX_NOISE_BANDS as usize;
    if bands.len() > max {
        return Err(ViolationKind::TooManyEntries {
            field,
            len: bands.len(),
            max,
        });
    }
    let mut ids = HashSet::with_capacity(bands.len());
    for b in bands {
        if b.band_id == 0 || !ids.insert(b.band_id) {
            return Err(bounds("measurements_started.noise_bands.band_id"));
        }
        check_len(
            "measurements_started.noise_bands.key",
            &b.key,
            limits::MAX_BAND_KEY_BYTES,
        )?;
        if !is_band_key(&b.key) {
            return Err(bounds("measurements_started.noise_bands.key"));
        }
        check_ranges("measurements_started.noise_bands.ranges", &b.ranges)?;
    }
    Ok(())
}

/// `MeasurementsStarted`, without the request it answers.
pub(crate) fn check_measurements_started(
    m: &v1::MeasurementsStarted,
    minor: u32,
) -> Result<(), ViolationKind> {
    check_enum::<v1::MeasurementDuty>("measurements_started.duty", m.duty, minor, true)?;
    check_band_list("measurements_started.noise_bands", &m.noise_bands)?;
    if !limits::NOISE_INTERVALS_MS.contains(&m.noise_interval_ms) {
        return Err(bounds("measurements_started.noise_interval_ms"));
    }
    let frontend = m
        .frontend
        .as_ref()
        .ok_or(missing("measurements_started.frontend"))?;
    check_frontend_state(frontend)
}

/// `NoiseFloorReport`, without its plan.
pub(crate) fn check_noise_report(n: &v1::NoiseFloorReport) -> Result<(), ViolationKind> {
    if !(1..=*limits::NOISE_INTERVALS_MS.last().unwrap_or(&300_000)).contains(&n.window_ms) {
        return Err(bounds("noise.window_ms"));
    }
    let frontend = n.frontend.as_ref().ok_or(missing("noise.frontend"))?;
    check_frontend_state(frontend)?;
    if n.clock.is_none() {
        return Err(missing("noise.clock"));
    }
    if ![1, 2, 5].contains(&n.cycle_minutes) || n.cycle_index >= n.cycle_minutes {
        return Err(bounds("noise.cycle_minutes"));
    }
    let max = limits::MAX_NOISE_BANDS as usize;
    if n.bands.len() > max {
        return Err(ViolationKind::TooManyEntries {
            field: "noise.bands",
            len: n.bands.len(),
            max,
        });
    }
    let mut ids = HashSet::with_capacity(n.bands.len());
    for b in &n.bands {
        if !ids.insert(b.band_id) {
            return Err(bounds("noise.bands.band_id"));
        }
        let r = b.measured.as_ref().ok_or(missing("noise.bands.measured"))?;
        if r.start_hz >= r.end_hz || r.end_hz > limits::MAX_FREQUENCY_HZ {
            return Err(ViolationKind::InvalidRange {
                field: "noise.bands.measured",
            });
        }
        check_finite("noise.bands.resolution_bw_hz", b.resolution_bw_hz)?;
        if b.resolution_bw_hz <= 0.0 || b.resolution_bw_hz > 1e7 {
            return Err(bounds("noise.bands.resolution_bw_hz"));
        }
        if b.samples > limits::MAX_NOISE_SAMPLES {
            return Err(bounds("noise.bands.samples"));
        }
        let q = [b.p10_db_hz, b.p25_db_hz, b.p50_db_hz, b.p90_db_hz];
        for v in q {
            check_finite("noise.bands.quantiles", v)?;
            if !limits::NOISE_DB_HZ.contains(&v) {
                return Err(bounds("noise.bands.quantiles"));
            }
        }
        if !q.windows(2).all(|w| w[0] <= w[1]) {
            return Err(bounds("noise.bands.quantiles"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ stateful

/// A band the hub asked for, or the station accepted.
#[derive(Debug, Clone)]
struct Band {
    /// `[start, end]` of each range.
    ranges: Vec<(u64, u64)>,
}

impl Band {
    fn from_proto(b: &v1::NoiseBand) -> Self {
        Self {
            ranges: b.ranges.iter().map(|r| (r.start_hz, r.end_hz)).collect(),
        }
    }

    /// Whether `[start, end]` lies inside one of the ranges.
    fn holds(&self, start: u64, end: u64) -> bool {
        self.ranges.iter().any(|(s, e)| *s <= start && end <= *e)
    }

    /// The lowest and highest frequency of the band.
    fn hull(&self) -> Option<(u64, u64)> {
        Some((self.ranges.first()?.0, self.ranges.last()?.1))
    }
}

#[derive(Debug)]
struct Accepted {
    bands: HashMap<u32, Band>,
    initial_duty: v1::MeasurementDuty,
}

#[derive(Debug)]
struct Plan {
    closed_at: Option<Instant>,
    requested: HashMap<u32, Band>,
    interval_ms: u32,
    accepted: Option<Accepted>,
    /// Duty changes, by the UTC minute they apply from (ms).
    duties: Vec<(i64, v1::MeasurementDuty)>,
    noise: TokenBucket,
    noise_excess: Sustained,
    duty: TokenBucket,
    duty_excess: Sustained,
}

impl Plan {
    fn duty_at(&self, unix_ms: i64) -> Option<v1::MeasurementDuty> {
        let initial = self.accepted.as_ref()?.initial_duty;
        Some(
            self.duties
                .iter()
                .rev()
                .find(|(from, _)| *from <= unix_ms)
                .map_or(initial, |(_, d)| *d),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdState {
    Open,
    Grace,
    Unknown,
}

/// The plans of one connection.
#[derive(Debug, Default)]
pub(crate) struct Plans {
    plans: HashMap<u32, Plan>,
}

/// What [`Plans::on_start_sent`] refuses: a hub bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InUse(pub u32);

fn noise_bucket(interval_ms: u32, now: Instant) -> TokenBucket {
    // §5.2: ≤ 1 per noise_interval_ms / 2, burst 2.
    let per_sec = 2000.0 / f64::from(interval_ms.max(1));
    TokenBucket::new(per_sec, 2.0, now)
}

fn within(unix_ms: i64, now_ms: i64, before: Duration, after: Duration) -> bool {
    let ms = |d: Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
    unix_ms >= now_ms.saturating_sub(ms(before)) && unix_ms <= now_ms.saturating_add(ms(after))
}

impl Plans {
    fn state(&self, id: u32, now: Instant) -> IdState {
        match self.plans.get(&id).map(|p| p.closed_at) {
            Some(None) => IdState::Open,
            Some(Some(t)) if now.saturating_duration_since(t) <= limits::ID_CLOSE_GRACE => {
                IdState::Grace
            }
            _ => IdState::Unknown,
        }
    }

    pub(crate) fn is_open(&self, id: u32, now: Instant) -> bool {
        self.state(id, now) == IdState::Open
    }

    /// The hub sends `StartMeasurements`: the plan opens.
    pub(crate) fn on_start_sent(
        &mut self,
        s: &v1::StartMeasurements,
        now: Instant,
    ) -> Result<(), InUse> {
        self.collect_garbage(now);
        if self.state(s.measurement_id, now) == IdState::Open {
            return Err(InUse(s.measurement_id));
        }
        let interval_ms = s.noise_interval_ms.max(1);
        self.plans.insert(
            s.measurement_id,
            Plan {
                closed_at: None,
                requested: s
                    .noise_bands
                    .iter()
                    .map(|b| (b.band_id, Band::from_proto(b)))
                    .collect(),
                interval_ms,
                accepted: None,
                duties: Vec::new(),
                noise: noise_bucket(interval_ms, now),
                noise_excess: Sustained::default(),
                duty: TokenBucket::new(
                    1.0 / limits::DUTY_EVENT_INTERVAL.as_secs_f64(),
                    f64::from(limits::DUTY_EVENT_BURST),
                    now,
                ),
                duty_excess: Sustained::default(),
            },
        );
        Ok(())
    }

    /// The hub sends `StopMeasurements`: the plan closes, with the grace.
    pub(crate) fn on_stop_sent(&mut self, id: u32, now: Instant) {
        self.close(id, now);
    }

    fn close(&mut self, id: u32, now: Instant) {
        if let Some(p) = self.plans.get_mut(&id) {
            p.closed_at.get_or_insert(now);
        }
    }

    /// A non-`OK` answer to `StartMeasurements`: the station has no such plan.
    pub(crate) fn on_refused(&mut self, id: u32) {
        self.plans.remove(&id);
    }

    /// `MeasurementsStarted` (statelessly valid) for the plan `id` the hub
    /// sent: it must echo the request.
    pub(crate) fn on_started(
        &mut self,
        id: u32,
        m: &v1::MeasurementsStarted,
        now: Instant,
    ) -> Result<(), &'static str> {
        if m.measurement_id != id {
            return Err("measurements_started.measurement_id");
        }
        let Some(plan) = self.plans.get_mut(&id) else {
            // Stopped and forgotten meanwhile: nothing to check against.
            return Ok(());
        };
        let mut bands = HashMap::with_capacity(m.noise_bands.len());
        for b in &m.noise_bands {
            let Some(asked) = plan.requested.get(&b.band_id) else {
                return Err("measurements_started.noise_bands.band_id");
            };
            if !b.ranges.iter().all(|r| asked.holds(r.start_hz, r.end_hz)) {
                return Err("measurements_started.noise_bands.ranges");
            }
            bands.insert(b.band_id, Band::from_proto(b));
        }
        if m.noise_interval_ms < plan.interval_ms {
            return Err("measurements_started.noise_interval_ms");
        }
        let Some(initial_duty) = duty_of(m.duty) else {
            return Err("measurements_started.duty");
        };
        plan.interval_ms = m.noise_interval_ms;
        plan.noise = noise_bucket(m.noise_interval_ms, now);
        plan.accepted = Some(Accepted {
            bands,
            initial_duty,
        });
        Ok(())
    }

    /// A statelessly valid `NoiseFloorReport`.
    pub(crate) fn check_noise(
        &mut self,
        n: &v1::NoiseFloorReport,
        now: Instant,
        unix_ms: Option<i64>,
    ) -> Result<Option<DropReason>, ViolationKind> {
        let kind = MessageKind::Noise;
        let id = n.measurement_id;
        match self.state(id, now) {
            IdState::Unknown => {
                return Err(ViolationKind::UnknownMeasurement {
                    kind,
                    measurement_id: id,
                });
            }
            IdState::Grace => return Ok(Some(DropReason::RecentlyClosed { kind, id })),
            IdState::Open => {}
        }
        let Some(plan) = self.plans.get_mut(&id) else {
            return Err(ViolationKind::UnknownMeasurement {
                kind,
                measurement_id: id,
            });
        };
        let Some(accepted) = plan.accepted.as_ref() else {
            return Ok(Some(DropReason::AwaitingMeasurements {
                measurement_id: id,
            }));
        };
        for b in &n.bands {
            let Some(band) = accepted.bands.get(&b.band_id) else {
                return Err(bounds("noise.bands.band_id"));
            };
            let r = b.measured.unwrap_or_default();
            let inside = band
                .hull()
                .is_some_and(|(lo, hi)| lo <= r.start_hz && r.end_hz <= hi);
            if !inside {
                return Err(ViolationKind::InvalidRange {
                    field: "noise.bands.measured",
                });
            }
        }
        if !plan.noise.try_take(now) {
            let busy = Duration::from_millis(u64::from(plan.interval_ms / 2));
            if plan.noise_excess.on_excess(
                now,
                busy,
                limits::SUSTAINED_GAP,
                limits::NOISE_RATE_SUSTAINED,
            ) {
                return Err(ViolationKind::MeasurementRate {
                    kind,
                    measurement_id: id,
                });
            }
            return Ok(Some(DropReason::RateLimited { kind }));
        }
        let start = n.window_start_unix_ms;
        if let Some(now_ms) = unix_ms {
            let whole = start.rem_euclid(i64::from(limits::NOISE_WINDOW_MS)) == 0;
            if !whole
                || !within(
                    start,
                    now_ms,
                    limits::NOISE_REPORT_MAX_AGE,
                    limits::NOISE_REPORT_MAX_LEAD,
                )
            {
                return Ok(Some(DropReason::MeasurementTime { kind }));
            }
        }
        if plan.duty_at(start) == Some(v1::MeasurementDuty::Idle) && !is_measuring_minute(start) {
            return Ok(Some(DropReason::NotScheduled));
        }
        Ok(None)
    }

    /// A statelessly valid `Event{MEASUREMENT_STOPPED}` or
    /// `Event{MEASUREMENT_DUTY}`. `Ok(None)` accepts it.
    pub(crate) fn check_event(
        &mut self,
        e: &v1::Event,
        now: Instant,
        unix_ms: Option<i64>,
    ) -> Result<Option<DropReason>, ViolationKind> {
        use v1::event::Kind;
        let kind = MessageKind::Event;
        let Some(id) = e.measurement_id else {
            return Ok(None);
        };
        match self.state(id, now) {
            IdState::Unknown => {
                return Err(ViolationKind::UnknownMeasurement {
                    kind,
                    measurement_id: id,
                });
            }
            IdState::Grace => return Ok(Some(DropReason::RecentlyClosed { kind, id })),
            IdState::Open => {}
        }
        match Kind::try_from(e.kind) {
            Ok(Kind::MeasurementStopped) => {
                self.close(id, now);
                Ok(None)
            }
            Ok(Kind::MeasurementDuty) => {
                let Some(plan) = self.plans.get_mut(&id) else {
                    return Ok(None);
                };
                if !plan.duty.try_take(now) {
                    if plan.duty_excess.on_excess(
                        now,
                        limits::DUTY_EVENT_INTERVAL,
                        limits::SUSTAINED_GAP,
                        limits::DUTY_RATE_SUSTAINED,
                    ) {
                        return Err(ViolationKind::MeasurementRate {
                            kind,
                            measurement_id: id,
                        });
                    }
                    return Ok(Some(DropReason::RateLimited { kind }));
                }
                let (Some(from), Some(duty)) =
                    (e.effective_unix_ms, e.measurement_duty.and_then(duty_of))
                else {
                    // `check_message` guarantees both; unreachable.
                    return Ok(None);
                };
                let whole = from.rem_euclid(60_000) == 0;
                let plausible = unix_ms.is_none_or(|now_ms| {
                    within(from, now_ms, limits::DUTY_MAX_AGE, limits::DUTY_MAX_LEAD)
                });
                if !whole || !plausible {
                    return Ok(Some(DropReason::MeasurementTime { kind }));
                }
                let at = plan.duties.partition_point(|(f, _)| *f <= from);
                plan.duties.insert(at, (from, duty));
                // Bounded: a duty changes at most once a minute, and only the
                // last hour can still matter (15 min of report age).
                let keep_after = from.saturating_sub(60 * 60_000);
                if plan.duties.len() > 64 {
                    plan.duties.retain(|(f, _)| *f >= keep_after);
                    plan.duties.truncate(64);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn collect_garbage(&mut self, now: Instant) {
        self.plans.retain(|_, p| {
            p.closed_at
                .is_none_or(|t| now.saturating_duration_since(t) <= limits::ID_CLOSE_GRACE)
        });
    }
}
