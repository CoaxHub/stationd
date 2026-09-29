// SPDX-License-Identifier: MIT
//! Token buckets and "sustained excess" tracking for the §5 rate limits.

use std::time::{Duration, Instant};

/// A token bucket over an injected clock. Time going backwards is treated as
/// no time passing.
#[derive(Debug, Clone)]
pub(crate) struct TokenBucket {
    rate_per_sec: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket.
    pub(crate) fn new(rate_per_sec: f64, capacity: f64, now: Instant) -> Self {
        let capacity = if capacity.is_finite() {
            capacity.max(1.0)
        } else {
            1.0
        };
        let rate_per_sec = if rate_per_sec.is_finite() {
            rate_per_sec.max(0.0)
        } else {
            0.0
        };
        Self {
            rate_per_sec,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    /// Takes one token if available.
    pub(crate) fn try_take(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Takes `n` tokens unconditionally (the balance may go negative: debt)
    /// and returns the balance afterwards.
    pub(crate) fn take_debt(&mut self, n: f64, now: Instant) -> f64 {
        self.refill(now);
        self.tokens -= n;
        self.tokens
    }

    pub(crate) fn rate_per_sec(&self) -> f64 {
        self.rate_per_sec
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if now > self.last {
            self.last = now;
        }
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.capacity);
    }
}

/// Tracks an episode of excess (drops or throttling). An episode continues
/// while each excess happens within `gap` of the end of the previous one
/// (`busy_until`), and is "sustained" once it has lasted `threshold`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sustained {
    episode_start: Option<Instant>,
    busy_until: Option<Instant>,
}

impl Sustained {
    /// Records one excess at `now`, which keeps the sender busy until
    /// `now + busy`. Returns true once the episode has lasted `threshold`.
    pub(crate) fn on_excess(
        &mut self,
        now: Instant,
        busy: Duration,
        gap: Duration,
        threshold: Duration,
    ) -> bool {
        let continues = self
            .busy_until
            .is_some_and(|until| now <= until.checked_add(gap).unwrap_or(until));
        let start = match self.episode_start {
            Some(start) if continues => start,
            _ => now,
        };
        self.episode_start = Some(start);
        self.busy_until = Some(now.checked_add(busy).unwrap_or(now));
        now.saturating_duration_since(start) >= threshold
    }
}

/// Tokens a media limiter at `per_sec` holds: [`crate::limits::MEDIA_BURST`]
/// worth, at least 2.
pub(crate) fn media_capacity(per_sec: f64) -> f64 {
    (per_sec * crate::limits::MEDIA_BURST.as_secs_f64()).max(2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills_and_caps() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(2.0, 2.0, t0);
        assert!(b.try_take(t0));
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0));
        assert!(b.try_take(t0 + Duration::from_millis(500)));
        assert!(!b.try_take(t0 + Duration::from_millis(500)));
        // Long idle: capped at capacity.
        let t1 = t0 + Duration::from_secs(100);
        assert!(b.try_take(t1));
        assert!(b.try_take(t1));
        assert!(!b.try_take(t1));
        // Time going backwards adds nothing.
        assert!(!b.try_take(t0));
        assert!((b.rate_per_sec() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bucket_sanitises_parameters() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(f64::NAN, f64::INFINITY, t0);
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0 + Duration::from_secs(10)));
        assert!(b.take_debt(3.0, t0) < 0.0);
    }

    #[test]
    fn sustained_episode() {
        let t0 = Instant::now();
        let gap = Duration::from_secs(1);
        let thr = Duration::from_secs(10);
        let mut s = Sustained::default();
        for i in 0..10 {
            assert!(!s.on_excess(t0 + Duration::from_secs(i), Duration::ZERO, gap, thr));
        }
        assert!(s.on_excess(t0 + Duration::from_secs(10), Duration::ZERO, gap, thr));
        // A pause longer than the gap starts a new episode.
        assert!(!s.on_excess(t0 + Duration::from_secs(12), Duration::ZERO, gap, thr));
    }
}
