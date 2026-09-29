// SPDX-License-Identifier: MIT
//! §5 Bandwidth: the per-station ingress quota.

use std::time::{Duration, Instant};

use super::rate::{Sustained, TokenBucket};
use crate::limits;

/// What the hub does with the stream after reading some bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    /// Within quota: keep reading.
    Allow,
    /// Over quota: wait this long before the next read (throttle reads).
    Throttle(Duration),
    /// Throttled for [`limits::INGRESS_SUSTAINED`]: send
    /// `Goodbye{QUOTA_EXCEEDED}` (not a strike).
    Exceeded,
}

/// Per-station ingress token bucket: 2 Mbit/s sustained, 4 Mbit burst
/// (station-protocol §5 Bandwidth).
///
/// The hub calls [`on_bytes`](Self::on_bytes) with the size of every frame
/// it reads from the station's stream. Bytes are always charged (the bucket
/// may go into debt); the decision says how long to pause reading so the
/// average stays at the quota.
#[derive(Debug, Clone)]
pub struct IngressQuota {
    bucket: TokenBucket,
    sustained: Sustained,
}

impl IngressQuota {
    /// A full bucket with the §5 parameters.
    pub fn new(now: Instant) -> Self {
        Self::with_limits(
            limits::INGRESS_RATE_BITS_PER_SECOND,
            limits::INGRESS_BURST_BITS,
            now,
        )
    }

    /// A full bucket with custom parameters (bits per second, burst bits),
    /// e.g. a plan-specific quota.
    #[allow(clippy::cast_precision_loss)] // bit rates are far below 2^52
    pub fn with_limits(rate_bits_per_second: u64, burst_bits: u64, now: Instant) -> Self {
        Self {
            bucket: TokenBucket::new(rate_bits_per_second as f64, burst_bits as f64, now),
            sustained: Sustained::default(),
        }
    }

    /// Charges `bytes` read at `now`.
    #[allow(clippy::cast_precision_loss)] // frame sizes are far below 2^52
    pub fn on_bytes(&mut self, bytes: usize, now: Instant) -> QuotaDecision {
        let balance = self.bucket.take_debt(bytes as f64 * 8.0, now);
        if balance >= 0.0 {
            return QuotaDecision::Allow;
        }
        let rate = self.bucket.rate_per_sec();
        let wait = if rate > 0.0 {
            Duration::try_from_secs_f64(-balance / rate).unwrap_or(limits::INGRESS_SUSTAINED)
        } else {
            limits::INGRESS_SUSTAINED
        };
        if self
            .sustained
            .on_excess(now, wait, limits::SUSTAINED_GAP, limits::INGRESS_SUSTAINED)
        {
            QuotaDecision::Exceeded
        } else {
            QuotaDecision::Throttle(wait)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_burst_is_allowed() {
        let t0 = Instant::now();
        let mut q = IngressQuota::new(t0);
        // 4 Mbit burst = 500 kB.
        for _ in 0..7 {
            assert_eq!(q.on_bytes(65_536, t0), QuotaDecision::Allow);
        }
        let QuotaDecision::Throttle(wait) = q.on_bytes(65_536, t0) else {
            panic!("expected throttle");
        };
        // Debt of 24_288 bytes at 250 kB/s.
        assert!(wait > Duration::from_millis(90) && wait < Duration::from_millis(110));
    }

    #[test]
    fn at_quota_forever_is_fine() {
        let t0 = Instant::now();
        let mut q = IngressQuota::new(t0);
        // 250 kB/s exactly, in 25 kB chunks every 100 ms, for 10 minutes.
        for i in 0..6000u64 {
            let now = t0 + Duration::from_millis(100 * i);
            assert_eq!(q.on_bytes(25_000, now), QuotaDecision::Allow);
        }
    }

    #[test]
    fn sustained_overload_is_exceeded_after_60s() {
        let t0 = Instant::now();
        let mut q = IngressQuota::new(t0);
        let mut now = t0;
        let mut exceeded_at = None;
        // 4 Mbit/s offered; the hub honours the throttle wait each time.
        for _ in 0..10_000 {
            match q.on_bytes(50_000, now) {
                QuotaDecision::Allow => now += Duration::from_millis(100),
                QuotaDecision::Throttle(wait) => now += wait.max(Duration::from_millis(100)),
                QuotaDecision::Exceeded => {
                    exceeded_at = Some(now);
                    break;
                }
            }
        }
        let at = exceeded_at.expect("sustained overload must be detected");
        let elapsed = at - t0;
        assert!(
            elapsed >= Duration::from_secs(60) && elapsed < Duration::from_secs(80),
            "{elapsed:?}"
        );
    }

    #[test]
    fn zero_rate_quota_throttles() {
        let t0 = Instant::now();
        let mut q = IngressQuota::with_limits(0, 8, t0);
        assert_eq!(q.on_bytes(1, t0), QuotaDecision::Allow);
        assert_eq!(
            q.on_bytes(1, t0),
            QuotaDecision::Throttle(limits::INGRESS_SUSTAINED)
        );
    }
}
