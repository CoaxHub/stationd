// SPDX-License-Identifier: GPL-3.0-only
//! The duty, the idle schedule and the pause rules (the measurement design §4, §4.1).
//! Pure functions over Unix milliseconds.
//!
//! - **Continuous** while the station has any listener channel or waterfall
//!   subscription open (the hub's lingering ones included): every minute is
//!   measured. **Idle** otherwise: only the measuring minutes (hh:00, hh:05,
//!   …). The duty is decided at each UTC minute boundary and changes at most
//!   once a minute.
//! - **Paused** (nothing measured, nothing reported) while the 1-minute load
//!   average is at least 0.75 × the CPU count, the outbound queue dropped
//!   audio or waterfall since the last check (a minute ago), or the backend
//!   reports a degraded state; it resumes after 5 minutes without any of them.

use station_proto::limits;
use station_proto::v1::MeasurementDuty;

/// One UTC minute, ms.
pub const MINUTE_MS: i64 = 60_000;

/// The load average, as a share of the CPU count, from which measuring
/// pauses (the measurement design §4, an open default).
pub const PAUSE_LOAD_PER_CPU: f32 = 0.75;

/// Quiet time before a paused plan resumes, ms.
pub const RESUME_AFTER_MS: i64 = 5 * MINUTE_MS;

/// The start of the minute containing `unix_ms`.
pub fn minute_of(unix_ms: i64) -> i64 {
    unix_ms.div_euclid(MINUTE_MS) * MINUTE_MS
}

/// The first minute boundary strictly after `unix_ms`.
pub fn next_minute(unix_ms: i64) -> i64 {
    minute_of(unix_ms) + MINUTE_MS
}

/// Whether the minute starting at `minute_ms` is a measuring minute.
pub fn is_measuring_minute(minute_ms: i64) -> bool {
    minute_ms
        .div_euclid(MINUTE_MS)
        .rem_euclid(limits::IDLE_PERIOD_MINUTES)
        == 0
}

/// Whether the minute starting at `minute_ms` is measured under `duty`.
pub fn is_measured(duty: MeasurementDuty, minute_ms: i64) -> bool {
    match duty {
        MeasurementDuty::Idle => is_measuring_minute(minute_ms),
        MeasurementDuty::Continuous | MeasurementDuty::Unspecified => true,
    }
}

/// The duty for a station with `open` listener channels and subscriptions.
pub fn duty_for(open: usize) -> MeasurementDuty {
    if open > 0 {
        MeasurementDuty::Continuous
    } else {
        MeasurementDuty::Idle
    }
}

/// What the pause rules look at, sampled at a minute boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Load {
    /// 1-minute load average.
    pub load_avg_1m: f32,
    /// CPUs (0 when unknown: the load rule is then off).
    pub cpu_count: u32,
    /// Audio frames and waterfall rows dropped by the outbound queue since
    /// stationd started.
    pub dropped_total: u64,
    /// The backend reports a degraded state.
    pub backend_degraded: bool,
}

/// The pause rules' state.
#[derive(Debug, Clone, Default)]
pub struct PauseGate {
    last_bad_ms: Option<i64>,
    dropped_total: Option<u64>,
}

impl PauseGate {
    /// Whether `load`, sampled at `now_ms`, is a reason to pause.
    fn is_bad(&mut self, load: Load) -> bool {
        #[allow(clippy::cast_precision_loss, reason = "CPU counts are small")]
        let busy = load.cpu_count > 0
            && load.load_avg_1m.is_finite()
            && load.load_avg_1m >= PAUSE_LOAD_PER_CPU * load.cpu_count as f32;
        let dropped = self
            .dropped_total
            .is_some_and(|before| load.dropped_total > before);
        self.dropped_total = Some(load.dropped_total);
        busy || dropped || load.backend_degraded
    }

    /// Whether measuring is paused at `now_ms`, without a new sample.
    pub fn is_paused(&self, now_ms: i64) -> bool {
        self.last_bad_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < RESUME_AFTER_MS)
    }

    /// Records the sample taken at `now_ms` and says whether measuring is
    /// paused for the minute that starts now.
    pub fn observe(&mut self, now_ms: i64, load: Load) -> bool {
        if self.is_bad(load) {
            self.last_bad_ms = Some(now_ms);
        }
        self.is_paused(now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-02 12:00 UTC.
    const NOON: i64 = 1_790_942_400_000;

    #[test]
    fn minutes() {
        assert_eq!(minute_of(NOON + 59_999), NOON);
        assert_eq!(next_minute(NOON), NOON + MINUTE_MS);
        assert_eq!(next_minute(NOON - 1), NOON);
        assert_eq!(minute_of(-1), -MINUTE_MS);
        assert!(is_measuring_minute(NOON));
        assert!(!is_measuring_minute(NOON + MINUTE_MS));
        assert!(is_measuring_minute(NOON + 55 * MINUTE_MS));
        assert!(is_measured(MeasurementDuty::Continuous, NOON + MINUTE_MS));
        assert!(!is_measured(MeasurementDuty::Idle, NOON + MINUTE_MS));
        assert!(is_measured(MeasurementDuty::Idle, NOON + 5 * MINUTE_MS));
        assert_eq!(duty_for(0), MeasurementDuty::Idle);
        assert_eq!(duty_for(2), MeasurementDuty::Continuous);
        // One measuring minute in five: 12 an hour.
        let n = (0..60)
            .filter(|m| is_measuring_minute(NOON + m * MINUTE_MS))
            .count();
        assert_eq!(n, 12);
    }

    #[test]
    fn pause_rules() {
        let calm = Load {
            load_avg_1m: 0.5,
            cpu_count: 4,
            dropped_total: 10,
            backend_degraded: false,
        };
        let mut g = PauseGate::default();
        // Drops before the first sample don't count.
        assert!(!g.observe(NOON, calm));
        assert!(!g.observe(NOON + MINUTE_MS, calm));
        // 3.0 on 4 CPUs pauses...
        let busy = Load {
            load_avg_1m: 3.0,
            ..calm
        };
        assert!(g.observe(NOON + 2 * MINUTE_MS, busy));
        // ...and it resumes after 5 quiet minutes.
        for m in 3..7 {
            assert!(g.observe(NOON + m * MINUTE_MS, calm), "minute {m}");
        }
        assert!(!g.observe(NOON + 7 * MINUTE_MS, calm));
        // Queue drops since the last check pause it.
        let dropped = Load {
            dropped_total: 11,
            ..calm
        };
        assert!(g.observe(NOON + 8 * MINUTE_MS, dropped));
        // A degraded backend too.
        let mut g = PauseGate::default();
        let degraded = Load {
            backend_degraded: true,
            ..calm
        };
        assert!(g.observe(NOON, degraded));
        // An unknown CPU count turns the load rule off.
        let mut g = PauseGate::default();
        let unknown = Load {
            load_avg_1m: 99.0,
            cpu_count: 0,
            ..calm
        };
        assert!(!g.observe(NOON, unknown));
    }
}
