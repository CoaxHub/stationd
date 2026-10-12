// SPDX-License-Identifier: MIT
//! The station's reconnection policy (station-protocol §2.2) as a pure state
//! machine: no clock, no sleeping, no global RNG.
//!
//! stationd calls [`Backoff::on_connected`] once the session is established
//! (`Welcome` received) and [`Backoff::on_disconnected`] after every failed
//! connection attempt or lost session, then sleeps for the returned delay.
//!
//! Base rule (§2.2): exponential backoff with full jitter. The cap starts at
//! [`limits::BACKOFF_INITIAL`] (1 s), doubles per consecutive failure up to
//! [`limits::BACKOFF_MAX`] (60 s); the delay is uniform in `[0, cap]`. The
//! count resets once a connection has lasted
//! [`limits::BACKOFF_RESET_AFTER`] (5 min). Restarting the hub therefore
//! never needs action on the station (NFR-5).
//!
//! One exception to the first cap: when an established session ends with no
//! `Goodbye` (the hub vanished) and no failure is pending, the first delay is
//! uniform in 0 to [`limits::RECONNECT_AFTER_DROP_MAX`] (5 s), which
//! spreads the reconnects of a restarted hub. The next failure continues
//! from the normal cap (2 s), doubling as usual. A failed attempt that never
//! reached `Welcome`, and a `Goodbye` with or without `retry_after_ms`, keep
//! the base rule.
//!
//! Per `Goodbye` reason:
//!
//! | Reason | Delay | Other |
//! |---|---|---|
//! | none (connect failure, stream lost), `UNSPECIFIED`, unknown | backoff + `retry_after_ms` | |
//! | `SHUTDOWN`, `DRAINING` | backoff + `retry_after_ms` | `retry_after_ms` spreads the reconnect storm |
//! | `REASSIGNED` (or `refresh_assignment`) | backoff + `retry_after_ms` | `refresh_assignment = true`: call `StationControl.GetAssignment` first |
//! | `PROTOCOL_VIOLATION`, `QUOTA_EXCEEDED`, `DUPLICATE_CONNECTION` | backoff + `retry_after_ms` | |
//! | `DISABLED` | max(5 min, `retry_after_ms`) | the owner may re-enable the station |
//! | `UNSUPPORTED_VERSION` | max(6 h, `retry_after_ms`) | log a prominent "update stationd" message |
//! | `REVOKED` | — | [`Decision::Stop`], permanently: re-enrollment needed |
//!
//! `retry_after_ms` is a minimum wait "on top of the backoff" (§2.2): it is
//! added to the jittered backoff, so the delay is never below it.

use std::time::{Duration, Instant};

use crate::limits;
use crate::v1;
use crate::v1::goodbye::Reason;

/// A source of uniformly distributed random numbers (injectable for tests).
///
/// Implemented for every `FnMut() -> u64`, e.g. `|| rand::random()`.
pub trait RandomSource {
    /// The next uniformly distributed `u64`.
    fn next_u64(&mut self) -> u64;
}

impl<F: FnMut() -> u64> RandomSource for F {
    fn next_u64(&mut self) -> u64 {
        self()
    }
}

/// The parameters of the policy; [`Policy::default`] is §2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// First backoff cap.
    pub initial: Duration,
    /// Largest backoff cap.
    pub max: Duration,
    /// Connected at least this long resets the backoff.
    pub reset_after: Duration,
    /// Cap of the first delay after an established session drops with no
    /// `Goodbye`.
    pub after_drop_max: Duration,
    /// Retry interval after `Goodbye{DISABLED}`.
    pub disabled_retry: Duration,
    /// Retry interval after `Goodbye{UNSUPPORTED_VERSION}`.
    pub unsupported_version_retry: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            initial: limits::BACKOFF_INITIAL,
            max: limits::BACKOFF_MAX,
            reset_after: limits::BACKOFF_RESET_AFTER,
            after_drop_max: limits::RECONNECT_AFTER_DROP_MAX,
            disabled_retry: limits::DISABLED_RETRY_INTERVAL,
            unsupported_version_retry: limits::UNSUPPORTED_VERSION_RETRY_INTERVAL,
        }
    }
}

/// What to do after a disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Wait `delay`, then connect again.
    Reconnect {
        /// How long to wait.
        delay: Duration,
        /// Call `StationControl.GetAssignment` before reconnecting.
        refresh_assignment: bool,
    },
    /// Do not reconnect: the credential was revoked (`Goodbye{REVOKED}`).
    /// Log an error and report `revoked` in `stationd status`.
    Stop,
}

/// The reconnection state machine.
#[derive(Debug, Clone)]
pub struct Backoff {
    policy: Policy,
    failures: u32,
    connected_since: Option<Instant>,
    revoked: bool,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Policy::default())
    }
}

impl Backoff {
    /// A fresh state machine.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            failures: 0,
            connected_since: None,
            revoked: false,
        }
    }

    /// The session was established at `now` (after `Welcome`).
    pub fn on_connected(&mut self, now: Instant) {
        self.connected_since = Some(now);
    }

    /// Consecutive failures counted so far (the exponent of the next cap).
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Whether a `REVOKED` goodbye stopped reconnection for good.
    pub fn is_revoked(&self) -> bool {
        self.revoked
    }

    /// The backoff cap the next jittered delay is drawn below.
    pub fn next_cap(&self) -> Duration {
        let factor = limits::BACKOFF_FACTOR
            .checked_pow(self.failures)
            .unwrap_or(u32::MAX);
        self.policy
            .initial
            .checked_mul(factor)
            .unwrap_or(self.policy.max)
            .min(self.policy.max)
    }

    /// A connection attempt failed or the session ended at `now`, with the
    /// hub's `Goodbye` if one was received.
    pub fn on_disconnected(
        &mut self,
        goodbye: Option<&v1::Goodbye>,
        now: Instant,
        rng: &mut impl RandomSource,
    ) -> Decision {
        if self.revoked {
            return Decision::Stop;
        }
        let was_established = match self.connected_since.take() {
            Some(since) => {
                if now.saturating_duration_since(since) >= self.policy.reset_after {
                    self.failures = 0;
                }
                true
            }
            None => false,
        };
        // An established session that ended with no `Goodbye` is a lost hub
        // (a crash or a restart): the first retry is spread over a wider
        // window. A consecutive failure (`failures > 0`) is not that first
        // retry and follows the exponential backoff.
        let dropped = was_established && goodbye.is_none() && self.failures == 0;
        let reason = goodbye.and_then(|g| Reason::try_from(g.reason).ok());
        let retry_after = goodbye.map_or(Duration::ZERO, |g| {
            Duration::from_millis(u64::from(g.retry_after_ms))
        });
        let refresh_assignment =
            goodbye.is_some_and(|g| g.refresh_assignment) || reason == Some(Reason::Reassigned);
        let delay = match reason {
            Some(Reason::Revoked) => {
                self.revoked = true;
                return Decision::Stop;
            }
            Some(Reason::Disabled) => self.policy.disabled_retry.max(retry_after),
            Some(Reason::UnsupportedVersion) => {
                self.policy.unsupported_version_retry.max(retry_after)
            }
            Some(
                Reason::Unspecified
                | Reason::Shutdown
                | Reason::Draining
                | Reason::Reassigned
                | Reason::ProtocolViolation
                | Reason::QuotaExceeded
                | Reason::DuplicateConnection,
            )
            | None => {
                let cap = if dropped {
                    self.next_cap().max(self.policy.after_drop_max)
                } else {
                    self.next_cap()
                };
                let jitter = full_jitter(cap, rng);
                self.failures = self.failures.saturating_add(1);
                retry_after.saturating_add(jitter)
            }
        };
        Decision::Reconnect {
            delay,
            refresh_assignment,
        }
    }
}

/// Uniform in `[0, cap]`, millisecond resolution.
fn full_jitter(cap: Duration, rng: &mut impl RandomSource) -> Duration {
    let cap_ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX);
    let ms = match cap_ms.checked_add(1) {
        Some(n) => rng.next_u64() % n,
        None => rng.next_u64(),
    };
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goodbye(reason: Reason, retry_after_ms: u32) -> v1::Goodbye {
        v1::Goodbye {
            reason: reason as i32,
            retry_after_ms,
            ..Default::default()
        }
    }

    fn max_rng() -> impl FnMut() -> u64 {
        || u64::MAX
    }

    fn delay(d: Decision) -> Duration {
        match d {
            Decision::Reconnect { delay, .. } => delay,
            Decision::Stop => panic!("unexpected stop"),
        }
    }

    #[test]
    fn caps_double_to_sixty_seconds() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        // `u64::MAX % (cap_ms + 1)` is not the cap itself; use cap_ms directly.
        let mut caps = Vec::new();
        for _ in 0..10 {
            let cap = b.next_cap();
            caps.push(cap.as_secs());
            let cap_ms = u64::try_from(cap.as_millis()).unwrap_or(0);
            let mut rng = move || cap_ms;
            assert_eq!(delay(b.on_disconnected(None, t0, &mut rng)), cap);
        }
        assert_eq!(caps, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(b.failures(), 10);
    }

    #[test]
    fn first_delay_after_a_dropped_session_is_drawn_below_five_seconds() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        b.on_connected(t0);
        // The largest draw: the cap itself, 5 s (not the 1 s base cap).
        let mut cap_draw = || 5_000;
        let d = delay(b.on_disconnected(None, t0 + Duration::from_secs(10), &mut cap_draw));
        assert_eq!(d, limits::RECONNECT_AFTER_DROP_MAX);
        // The spread reaches beyond the base cap, and zero is possible.
        let mut b = Backoff::default();
        b.on_connected(t0);
        let mut mid = || 3_500;
        assert_eq!(
            delay(b.on_disconnected(None, t0, &mut mid)),
            Duration::from_millis(3_500)
        );
        let mut b = Backoff::default();
        b.on_connected(t0);
        let mut zero = || 0;
        assert_eq!(
            delay(b.on_disconnected(None, t0, &mut zero)),
            Duration::ZERO
        );
        // A draw is reduced modulo the window: never above 5 s.
        let mut b = Backoff::default();
        b.on_connected(t0);
        let mut max = max_rng();
        assert!(delay(b.on_disconnected(None, t0, &mut max)) <= limits::RECONNECT_AFTER_DROP_MAX);
        // Later failures continue from the base: cap 2 s, then 4 s.
        assert_eq!(b.failures(), 1);
        assert_eq!(b.next_cap(), Duration::from_secs(2));
        let mut cap_draw = || 2_000;
        assert_eq!(
            delay(b.on_disconnected(None, t0, &mut cap_draw)),
            Duration::from_secs(2)
        );
        assert_eq!(b.next_cap(), Duration::from_secs(4));
    }

    #[test]
    fn dropped_session_after_a_long_run_gets_the_wide_window_again() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut rng = max_rng();
        for _ in 0..4 {
            b.on_disconnected(None, t0, &mut rng);
        }
        // Up for 5 minutes: the count resets, so this drop is a first one.
        b.on_connected(t0);
        let mut cap_draw = || 5_000;
        assert_eq!(
            delay(b.on_disconnected(None, t0 + limits::BACKOFF_RESET_AFTER, &mut cap_draw)),
            limits::RECONNECT_AFTER_DROP_MAX
        );
        // Up for less than 5 minutes after earlier failures: base backoff.
        let mut b = Backoff::default();
        b.on_disconnected(None, t0, &mut rng);
        b.on_connected(t0);
        let mut cap_draw = || 2_000;
        assert_eq!(
            delay(b.on_disconnected(None, t0 + Duration::from_secs(10), &mut cap_draw)),
            Duration::from_secs(2)
        );
        assert_eq!(b.next_cap(), Duration::from_secs(4));
    }

    #[test]
    fn a_goodbye_or_a_failed_attempt_keeps_the_base_window() {
        let t0 = Instant::now();
        // A failure before the session was established (no `Welcome`).
        let mut b = Backoff::default();
        let mut cap_draw = || 1_000;
        assert_eq!(
            delay(b.on_disconnected(None, t0, &mut cap_draw)),
            Duration::from_secs(1)
        );
        // The same draw modulo the base window: 5 s becomes 5000 % 1001.
        let mut b = Backoff::default();
        let mut five = || 5_000;
        assert_eq!(
            delay(b.on_disconnected(None, t0, &mut five)),
            Duration::from_millis(5_000 % 1_001)
        );
        // An established session ended by a refusal with `retry_after_ms`:
        // the base window plus the hub's wait.
        for reason in [Reason::Shutdown, Reason::Draining, Reason::Unspecified] {
            let mut b = Backoff::default();
            b.on_connected(t0);
            let mut five = || 5_000;
            assert_eq!(
                delay(b.on_disconnected(Some(&goodbye(reason, 7_000)), t0, &mut five)),
                Duration::from_millis(7_000 + 5_000 % 1_001),
                "{reason:?}"
            );
            // And with no `retry_after_ms` at all.
            let mut b = Backoff::default();
            b.on_connected(t0);
            assert_eq!(
                delay(b.on_disconnected(Some(&goodbye(reason, 0)), t0, &mut five)),
                Duration::from_millis(5_000 % 1_001),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn jitter_stays_within_zero_and_cap() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut xorshift = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let cap = b.next_cap();
            let d = delay(b.on_disconnected(None, t0, &mut xorshift));
            assert!(d <= cap, "{d:?} > {cap:?}");
        }
        let mut zero = || 0;
        assert_eq!(
            delay(b.on_disconnected(None, t0, &mut zero)),
            Duration::ZERO
        );
        let mut max = max_rng();
        assert!(delay(b.on_disconnected(None, t0, &mut max)) <= limits::BACKOFF_MAX);
    }

    #[test]
    fn resets_after_five_minutes_connected() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut rng = max_rng();
        for _ in 0..5 {
            b.on_disconnected(None, t0, &mut rng);
        }
        assert_eq!(b.next_cap(), Duration::from_secs(32));
        // Connected 4:59: no reset.
        b.on_connected(t0);
        b.on_disconnected(None, t0 + Duration::from_secs(299), &mut rng);
        assert_eq!(b.next_cap(), Duration::from_secs(60));
        // Connected exactly 5 min: reset.
        let t1 = t0 + Duration::from_secs(1000);
        b.on_connected(t1);
        b.on_disconnected(None, t1 + limits::BACKOFF_RESET_AFTER, &mut rng);
        assert_eq!(b.failures(), 1);
        assert_eq!(b.next_cap(), Duration::from_secs(2));
    }

    #[test]
    fn retry_after_is_added_to_the_backoff() {
        let t0 = Instant::now();
        for reason in [
            Reason::Shutdown,
            Reason::Draining,
            Reason::ProtocolViolation,
            Reason::QuotaExceeded,
            Reason::DuplicateConnection,
            Reason::Unspecified,
        ] {
            let mut b = Backoff::default();
            let mut zero = || 0;
            let d = b.on_disconnected(Some(&goodbye(reason, 7_000)), t0, &mut zero);
            assert_eq!(
                d,
                Decision::Reconnect {
                    delay: Duration::from_secs(7),
                    refresh_assignment: false
                },
                "{reason:?}"
            );
            let mut one = || 1000;
            // cap 2 s now; jitter 1000 % 2001 = 1000 ms.
            assert_eq!(
                delay(b.on_disconnected(Some(&goodbye(reason, 7_000)), t0, &mut one)),
                Duration::from_millis(8_000),
                "{reason:?}"
            );
            assert_eq!(b.failures(), 2, "{reason:?} counts as a failure");
        }
    }

    #[test]
    fn reassigned_refreshes_assignment() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut zero = || 0;
        assert_eq!(
            b.on_disconnected(Some(&goodbye(Reason::Reassigned, 0)), t0, &mut zero),
            Decision::Reconnect {
                delay: Duration::ZERO,
                refresh_assignment: true
            }
        );
        let flagged = v1::Goodbye {
            reason: Reason::Draining as i32,
            refresh_assignment: true,
            ..Default::default()
        };
        assert!(matches!(
            b.on_disconnected(Some(&flagged), t0, &mut zero),
            Decision::Reconnect {
                refresh_assignment: true,
                ..
            }
        ));
    }

    #[test]
    fn disabled_retries_every_five_minutes() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut rng = max_rng();
        for _ in 0..3 {
            assert_eq!(
                delay(b.on_disconnected(Some(&goodbye(Reason::Disabled, 0)), t0, &mut rng)),
                Duration::from_secs(300)
            );
        }
        assert_eq!(b.failures(), 0);
        // A larger retry_after wins.
        assert_eq!(
            delay(b.on_disconnected(Some(&goodbye(Reason::Disabled, 3_600_000)), t0, &mut rng)),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn unsupported_version_retries_every_six_hours() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut rng = max_rng();
        assert_eq!(
            delay(b.on_disconnected(Some(&goodbye(Reason::UnsupportedVersion, 10)), t0, &mut rng)),
            Duration::from_secs(6 * 3600)
        );
    }

    #[test]
    fn revoked_stops_for_good() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut rng = max_rng();
        assert_eq!(
            b.on_disconnected(Some(&goodbye(Reason::Revoked, 0)), t0, &mut rng),
            Decision::Stop
        );
        assert!(b.is_revoked());
        assert_eq!(b.on_disconnected(None, t0, &mut rng), Decision::Stop);
        b.on_connected(t0);
        assert_eq!(b.on_disconnected(None, t0, &mut rng), Decision::Stop);
    }

    #[test]
    fn unknown_reason_uses_backoff() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        let mut zero = || 0;
        let g = v1::Goodbye {
            reason: 999,
            retry_after_ms: 5,
            ..Default::default()
        };
        assert_eq!(
            delay(b.on_disconnected(Some(&g), t0, &mut zero)),
            Duration::from_millis(5)
        );
        assert_eq!(b.failures(), 1);
    }

    #[test]
    fn huge_policy_values_do_not_overflow() {
        let t0 = Instant::now();
        let mut b = Backoff::new(Policy {
            initial: Duration::MAX,
            max: Duration::MAX,
            ..Policy::default()
        });
        let mut rng = max_rng();
        for _ in 0..40 {
            let _ = b.on_disconnected(Some(&goodbye(Reason::Shutdown, u32::MAX)), t0, &mut rng);
        }
        assert_eq!(b.next_cap(), Duration::MAX);
    }
}
