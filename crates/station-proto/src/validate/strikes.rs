// SPDX-License-Identifier: MIT
//! §5: three strikes within 24 hours quarantine a station.

use std::collections::VecDeque;
use std::time::Instant;

use super::ViolationKind;
use crate::limits;

/// The result of recording a strike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrikeOutcome {
    /// Strikes within the window, including this one.
    pub count: usize,
    /// Whether the station is (now) quarantined.
    pub quarantined: bool,
}

/// Strike history of one station, over an injected clock.
///
/// Quarantine is sticky: once [`limits::STRIKES_TO_QUARANTINE`] strikes fall
/// within [`limits::STRIKE_WINDOW`], the station stays quarantined (refused
/// at connect time) until [`clear`](Self::clear) (owner or moderator action),
/// even after the strikes age out.
#[derive(Debug, Clone, Default)]
pub struct Strikes {
    recent: VecDeque<(Instant, &'static str)>,
    quarantined: bool,
}

impl Strikes {
    /// No strikes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a strike for `kind` at `now`. Violations that are not strikes
    /// ([`ViolationKind::is_strike`]) are ignored.
    pub fn record(&mut self, kind: &ViolationKind, now: Instant) -> StrikeOutcome {
        self.prune(now);
        if kind.is_strike() {
            self.recent.push_back((now, kind.code()));
            while self.recent.len() > limits::STRIKES_TO_QUARANTINE {
                self.recent.pop_front();
            }
            if self.recent.len() >= limits::STRIKES_TO_QUARANTINE {
                self.quarantined = true;
            }
        }
        StrikeOutcome {
            count: self.recent.len(),
            quarantined: self.quarantined,
        }
    }

    /// Strikes within the window ending at `now`.
    pub fn count(&self, now: Instant) -> usize {
        self.recent
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) < limits::STRIKE_WINDOW)
            .count()
    }

    /// Strike kinds ([`ViolationKind::code`]) within the window, oldest first.
    pub fn kinds(&self, now: Instant) -> Vec<&'static str> {
        self.recent
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) < limits::STRIKE_WINDOW)
            .map(|(_, k)| *k)
            .collect()
    }

    /// Whether the station is quarantined.
    pub fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    /// Clears the quarantine and the history (owner or moderator action).
    pub fn clear(&mut self) {
        self.recent.clear();
        self.quarantined = false;
    }

    fn prune(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) >= limits::STRIKE_WINDOW)
        {
            self.recent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const V: ViolationKind = ViolationKind::DuplicateHello;

    #[test]
    fn three_in_window_quarantines() {
        let t0 = Instant::now();
        let mut s = Strikes::new();
        assert_eq!(
            s.record(&V, t0),
            StrikeOutcome {
                count: 1,
                quarantined: false
            }
        );
        assert_eq!(
            s.record(&V, t0 + Duration::from_secs(3600)),
            StrikeOutcome {
                count: 2,
                quarantined: false
            }
        );
        let out = s.record(&ViolationKind::Decode, t0 + Duration::from_secs(7200));
        assert_eq!(
            out,
            StrikeOutcome {
                count: 3,
                quarantined: true
            }
        );
        assert!(s.is_quarantined());
        assert_eq!(
            s.kinds(t0 + Duration::from_secs(7200)),
            ["duplicate_hello", "duplicate_hello", "decode"]
        );
        // Sticky after strikes age out.
        let later = t0 + limits::STRIKE_WINDOW * 2;
        assert_eq!(s.count(later), 0);
        assert!(s.is_quarantined());
        s.clear();
        assert!(!s.is_quarantined());
        assert_eq!(s.count(later), 0);
    }

    #[test]
    fn strikes_age_out() {
        let t0 = Instant::now();
        let mut s = Strikes::new();
        s.record(&V, t0);
        s.record(&V, t0 + Duration::from_secs(1));
        // First strike exactly 24 h old no longer counts.
        let out = s.record(&V, t0 + limits::STRIKE_WINDOW);
        assert_eq!(
            out,
            StrikeOutcome {
                count: 2,
                quarantined: false
            }
        );
        assert_eq!(s.count(t0 + limits::STRIKE_WINDOW), 2);
    }

    #[test]
    fn unsupported_version_is_not_a_strike() {
        let t0 = Instant::now();
        let mut s = Strikes::new();
        let v = ViolationKind::UnsupportedVersion { major: 9, minor: 0 };
        for _ in 0..5 {
            assert_eq!(
                s.record(&v, t0),
                StrikeOutcome {
                    count: 0,
                    quarantined: false
                }
            );
        }
    }
}
