// SPDX-License-Identifier: GPL-3.0-only
//! `Hello`, `StationStatus` and host metrics (station-protocol §2, §9).
//!
//! Host metrics are best effort: whatever cannot be read from `/proc` or
//! `/sys` is reported as 0 or absent, never as a non-finite float (the hub
//! treats NaN/inf as a violation, §5).

use std::path::Path;

use station_proto::{PROTOCOL_VERSION, limits, v1};

use crate::dispatch::truncate_utf8;

/// `Hello.platform`: `<os>-<arch>`, e.g. `linux-aarch64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The first message on every stream (§2).
pub fn hello(backend: &v1::BackendInfo, boot_id: u64) -> v1::Hello {
    v1::Hello {
        protocol_version: Some(PROTOCOL_VERSION),
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        backend: Some(v1::BackendInfo {
            kind: backend.kind,
            version: truncate_utf8(&backend.version, limits::MAX_VERSION_BYTES),
        }),
        platform: truncate_utf8(&platform(), limits::MAX_PLATFORM_BYTES),
        boot_id,
    }
}

/// Counters that go into `StationStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// `stationd` uptime in seconds.
    pub uptime_s: u64,
    /// Open channels on this connection.
    pub active_channels: u32,
    /// Open subscriptions on this connection.
    pub active_subscriptions: u32,
    /// Audio frames dropped by the outbound queue since start.
    pub audio_frames_dropped_total: u64,
    /// Waterfall rows dropped by the outbound queue since start.
    pub waterfall_rows_dropped_total: u64,
    /// Measurement reports dropped by the outbound queue since start.
    pub measurement_reports_dropped_total: u64,
}

/// A front end's health the hub's checks pass: finite floats and a
/// specified state.
pub fn sanitize_frontend_health(mut f: v1::FrontendHealth) -> v1::FrontendHealth {
    if !f.gain_db.is_finite() {
        f.gain_db = 0.0;
    }
    f.input_power_dbfs = f.input_power_dbfs.filter(|p| p.is_finite());
    if f.state == v1::frontend_health::State::Unspecified as i32 {
        f.state = v1::frontend_health::State::Ok as i32;
    }
    f
}

/// A `StationStatus` from each receiver's health (§4.2), host metrics and
/// counters.
pub fn station_status(
    receivers: Vec<v1::ReceiverHealth>,
    host: v1::HostHealth,
    c: Counters,
) -> v1::StationStatus {
    v1::StationStatus {
        uptime_s: c.uptime_s,
        host: Some(host),
        receivers,
        active_channels: c.active_channels,
        active_subscriptions: c.active_subscriptions,
        audio_frames_dropped_total: c.audio_frames_dropped_total,
        waterfall_rows_dropped_total: c.waterfall_rows_dropped_total,
        measurement_reports_dropped_total: c.measurement_reports_dropped_total,
    }
}

/// Host metrics from `/proc` and `/sys` (Linux; zeros elsewhere).
pub fn host_health() -> v1::HostHealth {
    host_health_from(Path::new("/"))
}

/// [`host_health`] reading `proc/` and `sys/` under `root` (tests).
pub fn host_health_from(root: &Path) -> v1::HostHealth {
    let read = |p: &str| std::fs::read_to_string(root.join(p)).ok();
    let (mem_total_bytes, mem_available) = read("proc/meminfo")
        .map(|s| parse_meminfo(&s))
        .unwrap_or_default();
    v1::HostHealth {
        load_avg_1m: load_avg_1m_from(root),
        cpu_count: cpu_count(),
        cpu_temp_c: read("sys/class/thermal/thermal_zone0/temp")
            .and_then(|s| parse_millidegrees(&s)),
        mem_used_bytes: mem_total_bytes.saturating_sub(mem_available),
        mem_total_bytes,
        host_uptime_s: read("proc/uptime")
            .and_then(|s| parse_uptime(&s))
            .unwrap_or(0),
    }
}

/// The 1-minute load average from `proc/loadavg` under `root` (0 when it
/// can't be read).
pub fn load_avg_1m_from(root: &Path) -> f32 {
    std::fs::read_to_string(root.join("proc/loadavg"))
        .ok()
        .and_then(|s| parse_loadavg(&s))
        .unwrap_or(0.0)
}

/// The CPUs this process may use (0 when unknown).
pub fn cpu_count() -> u32 {
    std::thread::available_parallelism().map_or(0, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

fn finite(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}

/// First field of `/proc/loadavg`.
fn parse_loadavg(s: &str) -> Option<f32> {
    s.split_whitespace().next()?.parse().ok().and_then(finite)
}

/// `MemTotal` and `MemAvailable` of `/proc/meminfo`, in bytes.
fn parse_meminfo(s: &str) -> (u64, u64) {
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .map_or(0, |kib| kib.saturating_mul(1024))
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

/// First field of `/proc/uptime`, whole seconds.
fn parse_uptime(s: &str) -> Option<u64> {
    let secs: f64 = s.split_whitespace().next()?.parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        // Truncation to whole seconds is intended; u64 covers any uptime.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(secs as u64)
    } else {
        None
    }
}

/// A thermal zone reading in millidegrees Celsius.
fn parse_millidegrees(s: &str) -> Option<f32> {
    let milli: i32 = s.trim().parse().ok()?;
    // Readings are well within f32's exact integer range.
    #[allow(clippy::cast_precision_loss)]
    finite(milli as f32 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_fields() {
        let info = v1::BackendInfo {
            kind: v1::BackendKind::Ka9qRadio as i32,
            version: "v".repeat(100),
        };
        let h = hello(&info, 42);
        assert_eq!(h.protocol_version, Some(PROTOCOL_VERSION));
        assert_eq!(h.daemon_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(h.boot_id, 42);
        assert!(h.platform.starts_with(std::env::consts::OS));
        assert_eq!(
            h.backend.map(|b| b.version.len()),
            Some(limits::MAX_VERSION_BYTES)
        );
        // Passes the hub's first-message checks.
        let msg = v1::StationMessage {
            body: Some(v1::station_message::Body::Hello(hello(&info, 1))),
        };
        assert!(station_proto::validate::check_message(&msg, 0).is_ok());
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_loadavg("0.52 0.58 0.59 2/1234 5678\n"), Some(0.52));
        assert_eq!(parse_loadavg("NaN 1 1"), None);
        assert_eq!(parse_loadavg(""), None);
        let mem = "MemTotal:       16303424 kB\nMemFree:  100 kB\nMemAvailable:   8151712 kB\n";
        assert_eq!(parse_meminfo(mem), (16_303_424 * 1024, 8_151_712 * 1024));
        assert_eq!(parse_meminfo("garbage"), (0, 0));
        assert_eq!(parse_uptime("12345.67 99999.00\n"), Some(12345));
        assert_eq!(parse_uptime("-1 0"), None);
        assert_eq!(parse_uptime("inf 0"), None);
        assert_eq!(parse_uptime("x"), None);
        assert_eq!(parse_millidegrees("48312\n"), Some(48.312));
        assert_eq!(parse_millidegrees("hot"), None);
    }

    #[test]
    fn host_health_from_a_fake_root() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("proc")).expect("mkdir");
        std::fs::create_dir_all(root.join("sys/class/thermal/thermal_zone0")).expect("mkdir");
        std::fs::write(root.join("proc/loadavg"), "1.50 1 1 1/1 1").expect("w");
        std::fs::write(
            root.join("proc/meminfo"),
            "MemTotal: 1000 kB\nMemAvailable: 400 kB\n",
        )
        .expect("w");
        std::fs::write(root.join("proc/uptime"), "99.9 1").expect("w");
        std::fs::write(root.join("sys/class/thermal/thermal_zone0/temp"), "50000").expect("w");
        let h = host_health_from(root);
        assert!((h.load_avg_1m - 1.5).abs() < f32::EPSILON);
        assert_eq!(h.mem_total_bytes, 1_024_000);
        assert_eq!(h.mem_used_bytes, 614_400);
        assert_eq!(h.host_uptime_s, 99);
        assert_eq!(h.cpu_temp_c, Some(50.0));
        assert!(h.cpu_count >= 1);
        // Nothing readable: zeros, no temperature.
        let empty = tempfile::tempdir().expect("dir");
        let h = host_health_from(empty.path());
        assert_eq!(
            (h.mem_total_bytes, h.host_uptime_s, h.cpu_temp_c),
            (0, 0, None)
        );
        // The real host never yields non-finite values.
        assert!(host_health().load_avg_1m.is_finite());
    }

    #[test]
    fn status_carries_counters_and_sane_front_ends() {
        let c = Counters {
            uptime_s: 5,
            active_channels: 2,
            active_subscriptions: 1,
            audio_frames_dropped_total: 3,
            waterfall_rows_dropped_total: 4,
            measurement_reports_dropped_total: 5,
        };
        let rx = v1::ReceiverHealth {
            receiver_id: 3,
            state: v1::ReceiverState::Running as i32,
            ..Default::default()
        };
        let s = station_status(vec![rx.clone()], v1::HostHealth::default(), c);
        assert_eq!(s.uptime_s, 5);
        assert_eq!((s.active_channels, s.active_subscriptions), (2, 1));
        assert_eq!(
            (s.audio_frames_dropped_total, s.waterfall_rows_dropped_total),
            (3, 4)
        );
        assert_eq!(s.measurement_reports_dropped_total, 5);
        assert_eq!(s.receivers, vec![rx]);
        let f = sanitize_frontend_health(v1::FrontendHealth {
            gain_db: f32::NAN,
            input_power_dbfs: Some(f32::INFINITY),
            ..Default::default()
        });
        assert!(f.gain_db.is_finite());
        assert_eq!(f.input_power_dbfs, None);
        assert_eq!(f.state, v1::frontend_health::State::Ok as i32);
    }
}
