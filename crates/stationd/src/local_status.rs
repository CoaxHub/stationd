// SPDX-License-Identifier: GPL-3.0-only
//! `stationd status` and the runtime status file the daemon keeps.
//!
//! `stationd run` writes `status.json` into the runtime directory
//! (`/run/coaxhub/stationd/` in packaged builds, the state directory in
//! development builds) whenever the tunnel state changes and every
//! [`STATUS_WRITE_INTERVAL`]: tunnel state, backend state, last error. It
//! holds nothing secret. `stationd status` combines it with the enrollment
//! state.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use station_backend::Backend;
use station_proto::v1;
use tokio::sync::watch;
use tracing::warn;

use crate::config::{Config, CredentialsMode};
use crate::credentials::{self, CredentialsError};
use crate::daemon::{ConnectionState, StopReason};
use crate::state::{self, StateDir};

/// How often the daemon refreshes `status.json` without a state change.
pub const STATUS_WRITE_INTERVAL: Duration = Duration::from_secs(10);

/// Mode of `status.json` (nothing secret in it).
pub const STATUS_MODE: u32 = 0o644;

/// Seconds since the Unix epoch now.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds (proleptic Gregorian, UTC).
pub fn format_utc(unix_s: i64) -> String {
    let days = unix_s.div_euclid(86_400);
    let secs = unix_s.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// A duration in words: `29d 23h`, `5h 3m`, `42s`.
pub fn format_duration(secs: i64) -> String {
    let s = secs.unsigned_abs();
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {}s", s % 60)
    } else {
        format!("{s}s")
    }
}

/// The tunnel as `status.json` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tunnel {
    /// Dialing the gateway or waiting for `Welcome`.
    Connecting,
    /// Session established.
    Connected,
    /// Waiting before the next attempt.
    Backoff,
    /// Stopped on request.
    Stopped,
    /// The hub revoked the certificate (`Goodbye{REVOKED}`).
    Revoked,
}

/// What the daemon writes to `status.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    /// The daemon's process id.
    pub pid: u32,
    /// When this was written, Unix seconds.
    pub updated_unix_s: i64,
    /// The station ID.
    pub station_id: String,
    /// The gateway being dialed.
    pub gateway_url: String,
    /// The tunnel state.
    pub tunnel: Tunnel,
    /// The hub's connection id while connected.
    pub connection_id: Option<String>,
    /// The next retry delay while in backoff, milliseconds.
    pub backoff_ms: Option<u64>,
    /// The backend state (`running`, `degraded`, `down`, `unspecified`).
    pub backend: String,
    /// The backend version (e.g. the radiod commit).
    pub backend_version: String,
    /// Why the last attempt or session failed.
    pub last_error: Option<String>,
}

fn backend_state(state: v1::backend_health::State) -> &'static str {
    match state {
        v1::backend_health::State::Running => "running",
        v1::backend_health::State::Degraded => "degraded",
        v1::backend_health::State::Down => "down",
        v1::backend_health::State::Unspecified => "unspecified",
    }
}

impl RuntimeStatus {
    /// The status for `state`, `backend` and `last_error`.
    pub fn new(
        station_id: &str,
        gateway_url: &str,
        state: &ConnectionState,
        backend: &dyn Backend,
        last_error: Option<String>,
    ) -> Self {
        let (tunnel, connection_id, backoff_ms) = match state {
            ConnectionState::Connecting { .. } => (Tunnel::Connecting, None, None),
            ConnectionState::Connected { connection_id, .. } => {
                (Tunnel::Connected, Some(connection_id.clone()), None)
            }
            ConnectionState::Backoff { delay } => (
                Tunnel::Backoff,
                None,
                Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
            ),
            ConnectionState::Stopped(StopReason::Shutdown) => (Tunnel::Stopped, None, None),
            ConnectionState::Stopped(StopReason::Revoked) => (Tunnel::Revoked, None, None),
        };
        let health = backend.health();
        Self {
            pid: std::process::id(),
            updated_unix_s: unix_now(),
            station_id: station_id.to_owned(),
            gateway_url: gateway_url.to_owned(),
            tunnel,
            connection_id,
            backoff_ms,
            backend: backend_state(health.backend.state()).to_owned(),
            backend_version: backend.info().version,
            last_error,
        }
    }

    /// Writes `self` to `path` atomically.
    pub fn write(&self, path: &Path) -> Result<(), state::StateError> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| state::StateError {
            path: path.to_owned(),
            source: std::io::Error::other(e),
        })?;
        state::write_atomic(path, &json, STATUS_MODE)
    }

    /// Reads `path`; `None` if absent or unreadable.
    pub fn read(path: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }
}

/// Whether process `pid` exists (Linux `/proc`).
pub fn process_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Keeps `status.json` up to date until `state` reports `Stopped` or its
/// sender goes away. Write failures are logged once.
pub async fn keep_written(
    path: std::path::PathBuf,
    station_id: String,
    gateway_url: String,
    mut state: watch::Receiver<ConnectionState>,
    last_error: watch::Receiver<Option<String>>,
    backend: std::sync::Arc<dyn Backend>,
) {
    let mut warned = false;
    let mut tick = tokio::time::interval(STATUS_WRITE_INTERVAL);
    loop {
        let current = state.borrow_and_update().clone();
        let status = RuntimeStatus::new(
            &station_id,
            &gateway_url,
            &current,
            backend.as_ref(),
            last_error.borrow().clone(),
        );
        match status.write(&path) {
            Ok(()) => warned = false,
            Err(e) if !warned => {
                warn!(error = %e, "cannot write the runtime status file");
                warned = true;
            }
            Err(_) => {}
        }
        if matches!(current, ConnectionState::Stopped(_)) {
            return;
        }
        tokio::select! {
            r = state.changed() => if r.is_err() { return },
            _ = tick.tick() => {}
        }
    }
}

/// What `stationd status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// `static` or `enrolled`.
    pub credentials: &'static str,
    /// Whether usable credentials exist.
    pub enrolled: bool,
    /// Why not, when not.
    pub problem: Option<String>,
    /// The station ID.
    pub station_id: Option<String>,
    /// The certificate's `notAfter`, Unix seconds.
    pub cert_not_after_unix_s: Option<i64>,
    /// Seconds left on the certificate (negative when expired).
    pub cert_expires_in_s: Option<i64>,
    /// The home hub id (enrolled stations).
    pub hub_id: Option<String>,
    /// The gateway.
    pub gateway_url: Option<String>,
    /// The SDR profile.
    pub sdr_profile: Option<&'static str>,
    /// Whether a daemon is running (its `status.json` names a live process).
    pub daemon_running: bool,
    /// The daemon's last written status.
    pub daemon: Option<RuntimeStatus>,
}

/// Builds the status report for `config` at `now`.
pub fn report(config: &Config, now: i64) -> StatusReport {
    let mode = match config.credentials {
        CredentialsMode::Static(_) => "static",
        CredentialsMode::Enrolled => "enrolled",
    };
    let daemon = RuntimeStatus::read(&config.status_path());
    let daemon_running = daemon.as_ref().is_some_and(|d| process_alive(d.pid));
    let mut r = StatusReport {
        credentials: mode,
        enrolled: false,
        problem: None,
        station_id: None,
        cert_not_after_unix_s: None,
        cert_expires_in_s: None,
        hub_id: None,
        gateway_url: None,
        sdr_profile: config.sdr.map(|s| s.profile.name),
        daemon_running,
        daemon,
    };
    match credentials::load(config) {
        Ok(c) => {
            r.enrolled = true;
            r.station_id = Some(c.tls.station_id.to_string());
            r.cert_not_after_unix_s = Some(c.tls.validity.not_after_unix_s);
            r.cert_expires_in_s = Some(c.tls.validity.not_after_unix_s - now);
            r.hub_id = c.hub_id;
            r.gateway_url = Some(c.gateway.url);
        }
        Err(e) => {
            if let CredentialsError::NotEnrolled { .. } = e {
                r.problem = Some("not enrolled: run `stationd enroll`".to_owned());
            } else {
                r.problem = Some(e.to_string());
            }
            // Show what the state directory still tells.
            if let CredentialsMode::Enrolled = config.credentials {
                let dir = StateDir::new(&config.state_dir);
                r.station_id = dir.station_id().ok().map(|u| u.to_string());
                if let Ok(a) = dir.assignment() {
                    r.hub_id = Some(a.hub_id);
                    r.gateway_url = Some(a.gateway_url);
                }
            }
        }
    }
    r
}

impl StatusReport {
    /// The human-readable form.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |k: &str, v: &str| {
            let _ = writeln!(out, "{k:<12} {v}");
        };
        let enrolled = match (self.enrolled, self.credentials) {
            (true, "static") => "yes (static development credentials)".to_owned(),
            (true, _) => "yes".to_owned(),
            (false, _) => format!("no ({})", self.problem.as_deref().unwrap_or("unknown")),
        };
        line("enrolled", &enrolled);
        if let Some(id) = &self.station_id {
            line("station", id);
        }
        if let (Some(at), Some(left)) = (self.cert_not_after_unix_s, self.cert_expires_in_s) {
            let when = if left > 0 {
                format!("{} (in {})", format_utc(at), format_duration(left))
            } else {
                format!("{} (EXPIRED {} ago)", format_utc(at), format_duration(left))
            };
            line("certificate", &format!("expires {when}"));
        }
        if let Some(h) = &self.hub_id {
            line("home hub", h);
        }
        if let Some(g) = &self.gateway_url {
            line("gateway", g);
        }
        if let Some(p) = self.sdr_profile {
            line("sdr profile", p);
        }
        match (&self.daemon, self.daemon_running) {
            (None, _) => line("daemon", "no status (not running?)"),
            (Some(d), running) => {
                let tunnel = match d.tunnel {
                    Tunnel::Connecting => "connecting".to_owned(),
                    Tunnel::Connected => format!(
                        "connected (connection {})",
                        d.connection_id.as_deref().unwrap_or("?")
                    ),
                    Tunnel::Backoff => format!(
                        "waiting {} ms before reconnecting",
                        d.backoff_ms.unwrap_or(0)
                    ),
                    Tunnel::Stopped => "stopped".to_owned(),
                    Tunnel::Revoked => "revoked by the hub: enroll this station again".to_owned(),
                };
                let as_of = format_utc(d.updated_unix_s);
                if running {
                    line("daemon", &format!("running (pid {})", d.pid));
                } else {
                    line("daemon", &format!("not running (last status {as_of})"));
                }
                line("tunnel", &tunnel);
                line("backend", &format!("{} ({})", d.backend, d.backend_version));
                if let Some(e) = &d.last_error {
                    line("last error", &format!("{e:?}"));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_backend::FakeBackend;

    #[test]
    fn utc_and_durations() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(125), "2m 5s");
        assert_eq!(format_duration(3 * 3600 + 60), "3h 1m");
        assert_eq!(format_duration(-(30 * 86_400 + 3600)), "30d 1h");
        assert!(unix_now() > 1_700_000_000);
    }

    #[test]
    fn runtime_status_round_trip() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("status.json");
        assert_eq!(RuntimeStatus::read(&path), None);
        let backend = FakeBackend::default();
        let states = [
            (
                ConnectionState::Connecting { attempt: 1 },
                Tunnel::Connecting,
            ),
            (
                ConnectionState::Connected {
                    connection_id: "c1".into(),
                    version: v1::ProtocolVersion::default(),
                },
                Tunnel::Connected,
            ),
            (
                ConnectionState::Backoff {
                    delay: Duration::from_millis(1500),
                },
                Tunnel::Backoff,
            ),
            (
                ConnectionState::Stopped(StopReason::Shutdown),
                Tunnel::Stopped,
            ),
            (
                ConnectionState::Stopped(StopReason::Revoked),
                Tunnel::Revoked,
            ),
        ];
        for (state, tunnel) in states {
            let s = RuntimeStatus::new("id", "https://gw", &state, &backend, Some("boom".into()));
            assert_eq!(s.tunnel, tunnel);
            s.write(&path).expect("write");
            assert_eq!(RuntimeStatus::read(&path), Some(s));
        }
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(u32::MAX));
        assert!(
            RuntimeStatus::new(
                "id",
                "g",
                &ConnectionState::Connecting { attempt: 1 },
                &backend,
                None
            )
            .write(&tmp.path().join("no/dir/status.json"))
            .is_err()
        );
        for s in [
            v1::backend_health::State::Running,
            v1::backend_health::State::Degraded,
            v1::backend_health::State::Down,
            v1::backend_health::State::Unspecified,
        ] {
            assert!(!backend_state(s).is_empty());
        }
    }

    #[test]
    fn render_every_tunnel_state() {
        let base = StatusReport {
            credentials: "enrolled",
            enrolled: true,
            problem: None,
            station_id: Some("s".into()),
            cert_not_after_unix_s: Some(1_790_000_000),
            cert_expires_in_s: Some(86_400 * 20),
            hub_id: Some("sa1".into()),
            gateway_url: Some("https://gw".into()),
            sdr_profile: Some("rtlsdr"),
            daemon_running: false,
            daemon: None,
        };
        let text = base.render();
        assert!(text.contains("enrolled     yes\n"), "{text}");
        assert!(text.contains("in 20d 0h"), "{text}");
        assert!(text.contains("not running?"), "{text}");
        let expired = StatusReport {
            cert_expires_in_s: Some(-60),
            credentials: "static",
            ..base.clone()
        };
        let text = expired.render();
        assert!(text.contains("EXPIRED 1m 0s ago"), "{text}");
        assert!(text.contains("static development"), "{text}");
        let not = StatusReport {
            enrolled: false,
            problem: Some("not enrolled".into()),
            ..base.clone()
        };
        assert!(not.render().contains("no (not enrolled)"));
        for tunnel in [
            Tunnel::Connecting,
            Tunnel::Connected,
            Tunnel::Backoff,
            Tunnel::Stopped,
            Tunnel::Revoked,
        ] {
            let r = StatusReport {
                daemon_running: tunnel == Tunnel::Connected,
                daemon: Some(RuntimeStatus {
                    pid: 1,
                    updated_unix_s: 0,
                    station_id: "s".into(),
                    gateway_url: "g".into(),
                    tunnel,
                    connection_id: Some("c9".into()),
                    backoff_ms: Some(10),
                    backend: "running".into(),
                    backend_version: "fake-1".into(),
                    last_error: Some("x\ny".into()),
                }),
                ..base.clone()
            };
            let text = r.render();
            assert!(text.contains("tunnel"), "{text}");
            assert!(text.contains("x\\ny"), "control characters escaped: {text}");
        }
    }
}
