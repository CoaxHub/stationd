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

/// Per-station ingress token bucket (station-protocol §5 Bandwidth): 1 Mbit/s
/// plus 1 Mbit/s per running receiver sustained, with a burst of twice that
/// ([`limits::ingress_rate_bits_per_second`], [`limits::ingress_burst_bits`]).
///
/// The hub calls [`on_bytes`](Self::on_bytes) with the size of every frame
/// it reads from the station's stream, and
/// [`set_running_receivers`](Self::set_running_receivers) whenever the number
/// of running receivers changes (a new `Capabilities`). Bytes are always
/// charged (the bucket may go into debt); the decision says how long to pause
/// reading so the average stays at the quota.
#[derive(Debug, Clone)]
pub struct IngressQuota {
    bucket: TokenBucket,
    sustained: Sustained,
}

impl IngressQuota {
    /// A full bucket with the §5 parameters for `running_receivers`
    /// receivers.
    pub fn new(running_receivers: usize, now: Instant) -> Self {
        Self::with_limits(
            limits::ingress_rate_bits_per_second(running_receivers),
            limits::ingress_burst_bits(running_receivers),
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

    /// Rescales the quota to `running_receivers` receivers from `now` on.
    /// The balance carries over (capped at the new burst), so a change never
    /// hands out a fresh burst or forgives debt.
    #[allow(clippy::cast_precision_loss)] // bit rates are far below 2^52
    pub fn set_running_receivers(&mut self, running_receivers: usize, now: Instant) {
        self.bucket.set_limits(
            limits::ingress_rate_bits_per_second(running_receivers) as f64,
            limits::ingress_burst_bits(running_receivers) as f64,
            now,
        );
    }

    /// The sustained rate now, bits per second.
    pub fn rate_bits_per_second(&self) -> f64 {
        self.bucket.rate_per_sec()
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
        let mut q = IngressQuota::new(1, t0);
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
        let mut q = IngressQuota::new(1, t0);
        // 250 kB/s exactly, in 25 kB chunks every 100 ms, for 10 minutes.
        for i in 0..6000u64 {
            let now = t0 + Duration::from_millis(100 * i);
            assert_eq!(q.on_bytes(25_000, now), QuotaDecision::Allow);
        }
    }

    #[test]
    fn scales_with_running_receivers() {
        let t0 = Instant::now();
        // No receiver running: 1 Mbit/s, 2 Mbit (250 kB) burst.
        let mut q = IngressQuota::new(0, t0);
        assert!((q.rate_bits_per_second() - 1e6).abs() < 1.0);
        assert_eq!(q.on_bytes(250_000, t0), QuotaDecision::Allow);
        assert!(matches!(q.on_bytes(1, t0), QuotaDecision::Throttle(_)));

        // Three receivers: 4 Mbit/s, 8 Mbit (1 MB) burst. 500 kB/s for ten
        // minutes is within it.
        let mut q = IngressQuota::new(3, t0);
        for i in 0..6000u64 {
            let now = t0 + Duration::from_millis(100 * i);
            assert_eq!(q.on_bytes(50_000, now), QuotaDecision::Allow);
        }
        // ...but not within one receiver's quota.
        let mut q = IngressQuota::new(1, t0);
        let throttled = (0..6000u64).any(|i| {
            let now = t0 + Duration::from_millis(100 * i);
            q.on_bytes(50_000, now) != QuotaDecision::Allow
        });
        assert!(throttled);
    }

    #[test]
    fn rescaling_keeps_the_balance() {
        let t0 = Instant::now();
        let mut q = IngressQuota::new(1, t0);
        // Spend the 500 kB burst, then go from one receiver to six.
        assert_eq!(q.on_bytes(500_000, t0), QuotaDecision::Allow);
        q.set_running_receivers(6, t0);
        assert!((q.rate_bits_per_second() - 7e6).abs() < 1.0);
        // No fresh burst: the next byte is over.
        assert!(matches!(q.on_bytes(1_000, t0), QuotaDecision::Throttle(_)));
        // 7 Mbit/s refills 875 kB in a second.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(q.on_bytes(800_000, t1), QuotaDecision::Allow);
        // Down to none: the balance is capped at the smaller burst (250 kB).
        let t2 = t1 + Duration::from_secs(10);
        q.set_running_receivers(0, t2);
        assert_eq!(q.on_bytes(250_000, t2), QuotaDecision::Allow);
        assert!(matches!(q.on_bytes(1_000, t2), QuotaDecision::Throttle(_)));
    }

    #[test]
    fn sustained_overload_is_exceeded_after_60s() {
        let t0 = Instant::now();
        let mut q = IngressQuota::new(1, t0);
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
