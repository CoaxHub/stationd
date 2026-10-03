// SPDX-License-Identifier: GPL-3.0-only
//! The clocks of measuring: the wall clock the schedule follows
//! ([`MeasureClock`]), and the clock state the reports carry ([`clock_state`]).
//!
//! The schedule is aligned to UTC minutes, so it runs on the system clock
//! ([`SystemClock`]), which the packaged service only starts after the clock
//! is synchronised (`systemd-time-wait-sync`). Tests run it on a
//! [`ScaledClock`] over Tokio's clock (paused or sped up); that hook exists
//! only in the library's [`crate::RunOptions`], never in `stationd.toml`.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use station_proto::v1;

/// A wall clock for the measurement schedule, in Unix milliseconds.
pub trait MeasureClock: Send + Sync + fmt::Debug {
    /// Now.
    fn now_ms(&self) -> i64;

    /// Resolves at or after `unix_ms`.
    fn sleep_until(&self, unix_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

/// Unix milliseconds of the system clock.
pub fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl MeasureClock for SystemClock {
    fn now_ms(&self) -> i64 {
        system_now_ms()
    }

    fn sleep_until(&self, unix_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            // The wall clock may step (NTP): sleep in bounded pieces and look again.
            loop {
                let left = unix_ms.saturating_sub(system_now_ms());
                if left <= 0 {
                    return;
                }
                let left = u64::try_from(left).unwrap_or(0).min(10_000);
                tokio::time::sleep(Duration::from_millis(left)).await;
            }
        })
    }
}

/// A wall clock that starts at `base_ms` and runs `scale` times as fast as
/// Tokio's clock (tests: `scale` 1 under a paused clock is exact; 60 turns
/// a minute into a second).
#[derive(Debug, Clone, Copy)]
pub struct ScaledClock {
    base_ms: i64,
    base: tokio::time::Instant,
    scale: u32,
}

impl ScaledClock {
    /// Starts now at `base_ms`.
    pub fn new(base_ms: i64, scale: u32) -> Self {
        Self {
            base_ms,
            base: tokio::time::Instant::now(),
            scale: scale.max(1),
        }
    }

    /// The Tokio instant at which this clock reads `unix_ms`.
    pub fn instant_at(&self, unix_ms: i64) -> tokio::time::Instant {
        let ms = u64::try_from(unix_ms.saturating_sub(self.base_ms)).unwrap_or(0);
        self.base + Duration::from_micros(ms.saturating_mul(1000) / u64::from(self.scale))
    }

    /// What this clock read at Tokio instant `at`.
    pub fn ms_at(&self, at: tokio::time::Instant) -> i64 {
        let us = at.saturating_duration_since(self.base).as_micros();
        let ms = us.saturating_mul(u128::from(self.scale)) / 1000;
        self.base_ms
            .saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
    }
}

impl MeasureClock for ScaledClock {
    fn now_ms(&self) -> i64 {
        self.ms_at(tokio::time::Instant::now())
    }

    fn sleep_until(&self, unix_ms: i64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep_until(self.instant_at(unix_ms)))
    }
}

/// systemd-timesyncd's mark of a synchronised clock.
const TIMESYNC_SYNCHRONIZED: &str = "run/systemd/timesync/synchronized";
/// systemd-timesyncd's state directory: present when it runs.
const TIMESYNC_DIR: &str = "run/systemd/timesync";

/// The station's clock state for a report (`ClockState`).
///
/// `synchronized` is false only when systemd-timesyncd runs and has not
/// synchronised the clock (no `/run/systemd/timesync/synchronized`); without
/// timesyncd, the packaged unit's wait for time sync before start stands
/// for it. `max_error_ms` is 0: stationd can't read the kernel's maximum
/// error (`adjtimex`) without unsafe code. The hub's own offset estimate
/// (`Ping`/`Pong`) is the check that matters at minute resolution.
pub fn clock_state() -> v1::ClockState {
    clock_state_at(Path::new("/"))
}

/// [`clock_state`] under `root` (tests).
pub fn clock_state_at(root: &Path) -> v1::ClockState {
    let runs = root.join(TIMESYNC_DIR).is_dir();
    let synced = root.join(TIMESYNC_SYNCHRONIZED).exists();
    v1::ClockState {
        synchronized: !runs || synced,
        max_error_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn scaled_clock() {
        let c = ScaledClock::new(1_000_000, 60);
        assert_eq!(c.now_ms(), 1_000_000);
        c.sleep_until(1_060_000).await;
        // One minute of this clock is one second of Tokio's.
        assert_eq!(c.now_ms(), 1_060_000);
        assert_eq!(
            c.instant_at(1_120_000) - c.instant_at(1_060_000),
            Duration::from_secs(1)
        );
        // The past resolves at once.
        c.sleep_until(0).await;
        let exact = ScaledClock::new(5, 1);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(exact.now_ms(), 255);
    }

    #[tokio::test]
    async fn system_clock() {
        let c = SystemClock;
        let now = c.now_ms();
        assert!(now > 1_700_000_000_000);
        c.sleep_until(now - 1).await;
        c.sleep_until(c.now_ms() + 20).await;
        assert!(c.now_ms() >= now + 20);
    }

    #[test]
    fn clock_state_from_timesyncd() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(clock_state_at(dir.path()).synchronized, "no timesyncd");
        std::fs::create_dir_all(dir.path().join(TIMESYNC_DIR)).expect("mkdir");
        assert!(!clock_state_at(dir.path()).synchronized, "not synced yet");
        std::fs::write(dir.path().join(TIMESYNC_SYNCHRONIZED), b"").expect("touch");
        let s = clock_state_at(dir.path());
        assert!(s.synchronized);
        assert_eq!(s.max_error_ms, 0);
    }
}
