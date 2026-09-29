// SPDX-License-Identifier: GPL-3.0-only
//! CPU usage of this process and of radiod, from `/proc/<pid>/stat`.
//!
//! `utime` + `stime` (fields 14 and 15, man 5 proc) are the CPU time of
//! all threads of the process, in clock ticks of `USER_HZ`. `USER_HZ` is 100
//! on every Linux architecture we target (x86-64 and arm64); reading it
//! with `sysconf(_SC_CLK_TCK)` would need `unsafe` libc calls, which this
//! workspace forbids.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Clock ticks per second in `/proc/<pid>/stat` (Linux `USER_HZ`).
pub const USER_HZ: u64 = 100;

/// Parsed from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    /// Process state letter (`R`, `S`, `Z`, ...).
    pub state: char,
    /// `utime + stime`, in `USER_HZ` ticks.
    pub cpu_ticks: u64,
}

/// Parses the contents of `/proc/<pid>/stat`. The command name (field 2)
/// may contain spaces and parentheses, so fields are counted from the last
/// `)`.
pub fn parse_stat(contents: &str) -> Result<(String, ProcStat)> {
    let open = contents.find('(').context("no '(' in stat")?;
    let close = contents.rfind(')').context("no ')' in stat")?;
    if close < open {
        bail!("malformed stat");
    }
    let comm = contents[open + 1..close].to_owned();
    let rest: Vec<&str> = contents[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state); utime is field 14, stime field 15.
    let state = rest
        .first()
        .and_then(|s| s.chars().next())
        .context("no state field")?;
    let field = |i: usize| -> Result<u64> {
        rest.get(i)
            .context("stat has too few fields")?
            .parse::<u64>()
            .with_context(|| format!("stat field {} is not a number", i + 3))
    };
    let cpu_ticks = field(11)?.saturating_add(field(12)?);
    Ok((comm, ProcStat { state, cpu_ticks }))
}

fn stat_path(pid: Option<u32>) -> PathBuf {
    match pid {
        Some(pid) => PathBuf::from(format!("/proc/{pid}/stat")),
        None => PathBuf::from("/proc/self/stat"),
    }
}

/// Reads `/proc/<pid>/stat` (`None` = this process).
pub fn read_stat(pid: Option<u32>) -> Result<(String, ProcStat)> {
    let path = stat_path(pid);
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_stat(&contents)
}

/// CPU use as a percentage of one core over `wall`.
#[allow(
    clippy::cast_precision_loss,
    reason = "tick counts stay far below 2^52"
)]
pub fn percent_of_one_core(ticks: u64, wall: Duration) -> f64 {
    let secs = wall.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    ticks as f64 / USER_HZ as f64 / secs * 100.0
}

/// Pidfile locations of `scripts/run-radiod-siggen.sh`.
pub fn radiod_pidfiles(xdg_runtime_dir: Option<&str>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = xdg_runtime_dir.filter(|d| !d.is_empty()) {
        v.push(Path::new(dir).join("coaxhub-radiod/radiod.pid"));
    }
    v.push(PathBuf::from("/tmp/coaxhub-radiod/radiod.pid"));
    v
}

/// The pid of a live (non-zombie) radiod from the first readable pidfile.
pub fn find_radiod_pid(pidfiles: &[PathBuf]) -> Option<u32> {
    pidfiles.iter().find_map(|p| {
        let pid: u32 = std::fs::read_to_string(p).ok()?.trim().parse().ok()?;
        is_live_radiod(pid).then_some(pid)
    })
}

/// True if `pid` is a running radiod (not a zombie).
pub fn is_live_radiod(pid: u32) -> bool {
    matches!(read_stat(Some(pid)), Ok((comm, st)) if comm == "radiod" && st.state != 'Z')
}

/// Measures CPU of this process and optionally one other over a run.
#[derive(Debug)]
pub struct CpuMeter {
    start: std::time::Instant,
    own: u64,
    other: Option<(u32, u64)>,
}

/// Result of [`CpuMeter::finish`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuReport {
    /// Wall-clock duration.
    pub wall: Duration,
    /// This process, % of one core.
    pub own_percent: f64,
    /// The other process, % of one core, if it was measured.
    pub other_percent: Option<f64>,
}

impl CpuMeter {
    /// Starts measuring this process and `other` (if it can be read).
    pub fn start(other: Option<u32>) -> Result<Self> {
        let own = read_stat(None)?.1.cpu_ticks;
        let other = other.and_then(|pid| read_stat(Some(pid)).ok().map(|s| (pid, s.1.cpu_ticks)));
        Ok(Self {
            start: std::time::Instant::now(),
            own,
            other,
        })
    }

    /// Stops measuring.
    pub fn finish(&self) -> Result<CpuReport> {
        let wall = self.start.elapsed();
        let own = read_stat(None)?.1.cpu_ticks.saturating_sub(self.own);
        let other_percent = self.other.and_then(|(pid, before)| {
            let now = read_stat(Some(pid)).ok()?.1.cpu_ticks;
            Some(percent_of_one_core(now.saturating_sub(before), wall))
        });
        Ok(CpuReport {
            wall,
            own_percent: percent_of_one_core(own, wall),
            other_percent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "1234 (radi od) (x)) S 1 1234 1234 0 -1 4194560 2000 0 0 0 \
                          150 50 0 0 20 0 9 0 12345 100000000 8000 18446744073709551615";

    #[test]
    fn parses_stat_with_odd_command_names() {
        let (comm, st) = parse_stat(SAMPLE).unwrap();
        assert_eq!(comm, "radi od) (x)");
        assert_eq!(st.state, 'S');
        assert_eq!(st.cpu_ticks, 200);
    }

    #[test]
    fn rejects_malformed_stat() {
        assert!(parse_stat("").is_err());
        assert!(parse_stat("1 (x").is_err());
        assert!(parse_stat("1 x) (").is_err());
        assert!(parse_stat("1 (x) S 1 2").is_err());
        assert!(parse_stat("1 (x) S 1 2 3 4 5 6 7 8 9 10 abc 5").is_err());
        assert!(parse_stat("1 (x)").is_err());
    }

    #[test]
    fn reads_own_stat() {
        let (comm, st) = read_stat(None).unwrap();
        assert!(!comm.is_empty());
        assert_ne!(st.state, 'Z');
        assert!(read_stat(Some(u32::MAX)).is_err());
    }

    #[test]
    fn percent_math() {
        assert!((percent_of_one_core(50, Duration::from_secs(1)) - 50.0).abs() < 1e-9);
        assert!((percent_of_one_core(300, Duration::from_secs(2)) - 150.0).abs() < 1e-9);
        assert!(percent_of_one_core(10, Duration::ZERO).abs() < f64::EPSILON);
    }

    #[test]
    fn pidfile_discovery() {
        let v = radiod_pidfiles(Some("/run/user/1000"));
        assert_eq!(
            v[0],
            PathBuf::from("/run/user/1000/coaxhub-radiod/radiod.pid")
        );
        assert_eq!(v.len(), 2);
        assert_eq!(radiod_pidfiles(Some("")).len(), 1);
        assert_eq!(radiod_pidfiles(None).len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("a.pid");
        std::fs::write(&bogus, "not a pid").unwrap();
        // Our own pid is live but not radiod.
        let own = dir.path().join("b.pid");
        std::fs::write(&own, std::process::id().to_string()).unwrap();
        let missing = dir.path().join("c.pid");
        assert_eq!(find_radiod_pid(&[bogus, own, missing]), None);
        assert!(!is_live_radiod(std::process::id()));
    }

    #[test]
    fn meter_measures_this_process() {
        let meter = CpuMeter::start(Some(std::process::id())).unwrap();
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        let r = meter.finish().unwrap();
        assert!(r.own_percent >= 0.0);
        assert!(r.other_percent.is_some());
        assert!(r.wall > Duration::ZERO);
        let none = CpuMeter::start(Some(u32::MAX)).unwrap().finish().unwrap();
        assert_eq!(none.other_percent, None);
    }
}
