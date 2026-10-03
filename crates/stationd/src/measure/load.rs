// SPDX-License-Identifier: GPL-3.0-only
//! Where the pause rule reads the host's load (the measurement design §4.1: measuring
//! pauses while the 1-minute load average is at least 0.75 × the CPU count).
//!
//! A station reads its own: [`ProcLoad`], `/proc/loadavg` and the CPUs it
//! may use. Tests give the runner a [`FixedLoad`] instead, so that measuring
//! doesn't depend on how busy the machine running them is; the hook is the
//! library's [`crate::RunOptions::measure_load`]. Development builds also
//! accept `[measurements] ignore_host_load = true` in `stationd.toml` (the
//! end-to-end scripts on a busy machine), which packaged builds reject.

use std::fmt;
use std::path::Path;

/// The host's load, as the pause rule sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostLoad {
    /// The 1-minute load average.
    pub load_avg_1m: f32,
    /// CPUs (0 when unknown: the load rule is then off).
    pub cpu_count: u32,
}

/// A source of [`HostLoad`], sampled at each minute boundary.
pub trait LoadSource: Send + Sync + fmt::Debug {
    /// The load now.
    fn sample(&self) -> HostLoad;
}

/// The host's own load: `/proc/loadavg` (0 when it can't be read) and the
/// available parallelism.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcLoad;

impl LoadSource for ProcLoad {
    fn sample(&self) -> HostLoad {
        HostLoad {
            load_avg_1m: crate::status::load_avg_1m_from(Path::new("/")),
            cpu_count: crate::status::cpu_count(),
        }
    }
}

/// A load that never changes (tests; `ignore_host_load` in development
/// builds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedLoad(pub HostLoad);

impl FixedLoad {
    /// An idle host: no load on this machine's CPUs.
    pub fn idle() -> Self {
        Self::on_this_host(0.0)
    }

    /// `load_avg_1m` on this machine's CPUs.
    pub fn on_this_host(load_avg_1m: f32) -> Self {
        Self(HostLoad {
            load_avg_1m,
            cpu_count: crate::status::cpu_count(),
        })
    }
}

impl LoadSource for FixedLoad {
    fn sample(&self) -> HostLoad {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_load_is_what_it_was_given() {
        let high = FixedLoad(HostLoad {
            load_avg_1m: 9.0,
            cpu_count: 4,
        });
        assert_eq!(high.sample(), high.0);
        let idle = FixedLoad::idle().sample();
        assert!(idle.load_avg_1m.abs() < f32::EPSILON);
        assert_eq!(idle.cpu_count, crate::status::cpu_count());
    }

    #[test]
    fn proc_load_is_finite() {
        assert!(ProcLoad.sample().load_avg_1m.is_finite());
    }
}
