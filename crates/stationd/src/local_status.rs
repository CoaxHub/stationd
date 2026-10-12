// SPDX-License-Identifier: GPL-3.0-only
//! `stationd status` and the runtime status file the daemon keeps.
//!
//! `stationd run` writes `status.json` into the runtime directory
//! (`/run/coaxhub/stationd/` in packaged builds, the state directory in
//! development builds) whenever the tunnel state or what the daemon
//! observed changes, and every [`STATUS_WRITE_INTERVAL`]: tunnel state,
//! each receiver's state, last error, clock skew against the hub, and
//! certificate renewal. It holds nothing secret. `stationd status` combines
//! it with the enrollment state and the applied receivers.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::warn;

use crate::config::{Config, CredentialsMode};
use crate::credentials::{self, CredentialsError};
use crate::daemon::{ConnectionState, Observed, StopReason};
use crate::receivers::{ReceiverSummary, Receivers, StoredSet};
use crate::renew::RenewalStatus;
use crate::state::{self, StateDir};

/// Clock skew above which `status` and `doctor` warn (never an error).
pub const CLOCK_SKEW_WARN_MS: i64 = 30_000;

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
    /// The certificate was revoked (`Goodbye{REVOKED}` or `station-api`).
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
    /// The backend kind and version (e.g. the radiod commit).
    pub backend_version: String,
    /// The receivers and their states.
    pub receivers: Vec<ReceiverSummary>,
    /// Why the last attempt or session failed.
    pub last_error: Option<String>,
    /// Hub time minus local time at the last `Welcome`, milliseconds.
    pub clock_skew_ms: Option<i64>,
    /// Certificate renewal.
    pub renewal: RenewalStatus,
}

impl RuntimeStatus {
    /// The status for `state`, `observed`, `receivers` and `last_error`.
    pub fn new(
        station_id: &str,
        state: &ConnectionState,
        observed: &Observed,
        receivers: &Receivers,
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
        Self {
            pid: std::process::id(),
            updated_unix_s: unix_now(),
            station_id: station_id.to_owned(),
            gateway_url: observed.gateway_url.clone(),
            tunnel,
            connection_id,
            backoff_ms,
            backend_version: receivers.backend_info().version,
            receivers: receivers.summaries(),
            last_error,
            clock_skew_ms: observed.clock_skew_ms,
            renewal: observed.renewal.clone(),
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
/// sender goes away; a receiver's event writes it again too. Write failures
/// are logged once.
pub async fn keep_written(
    path: std::path::PathBuf,
    station_id: String,
    mut state: watch::Receiver<ConnectionState>,
    mut observed: watch::Receiver<Observed>,
    last_error: watch::Receiver<Option<String>>,
    receivers: Receivers,
) {
    let mut warned = false;
    let mut observing = true;
    let mut events = receivers.subscribe();
    let mut listening = true;
    let mut tick = tokio::time::interval(STATUS_WRITE_INTERVAL);
    loop {
        let current = state.borrow_and_update().clone();
        let seen = observed.borrow_and_update().clone();
        let status = RuntimeStatus::new(
            &station_id,
            &current,
            &seen,
            &receivers,
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
            r = observed.changed(), if observing => observing = r.is_ok(),
            r = events.recv(), if listening => {
                listening = !matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed));
            }
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
    /// The certificate's serial, hex.
    pub cert_serial: Option<String>,
    /// When the certificate is due for renewal (two thirds of its
    /// lifetime), Unix seconds.
    pub cert_renew_at_unix_s: Option<i64>,
    /// The home hub id (enrolled stations).
    pub hub_id: Option<String>,
    /// The gateway.
    pub gateway_url: Option<String>,
    /// The applied receivers: their states from the running daemon, else
    /// as `receivers.json` keeps them (state `applied`).
    pub receivers: Vec<ReceiverSummary>,
    /// Why `receivers.json` can't be used, when it can't.
    pub receivers_problem: Option<String>,
    /// Whether a daemon is running (its `status.json` names a live process).
    pub daemon_running: bool,
    /// The daemon's last written status.
    pub daemon: Option<RuntimeStatus>,
}

/// The receivers of `set` as applied, while no daemon reports their state.
pub fn applied(set: &StoredSet) -> Vec<ReceiverSummary> {
    set.receivers
        .iter()
        .map(|r| ReceiverSummary {
            receiver_id: r.receiver_id,
            state: "applied".to_owned(),
            detail: String::new(),
            profile: r.profile.to_string(),
            sdr_serial: r.sdr_serial.clone(),
            center_frequency_hz: r.center_frequency_hz,
            slot: r.slot,
        })
        .collect()
}

/// One receiver in one line: `running, rtlsdr-v4 at 14200000 Hz on SDR
/// A0000011 (slot 3)`, with the detail of a failure.
pub fn describe_receiver(r: &ReceiverSummary) -> String {
    let sdr = if r.sdr_serial.is_empty() {
        "the only RTL-SDR".to_owned()
    } else {
        format!("SDR {}", r.sdr_serial)
    };
    let mut text = format!(
        "{}, {} at {} Hz on {sdr} (slot {})",
        r.state, r.profile, r.center_frequency_hz, r.slot
    );
    if !r.detail.is_empty() {
        let _ = write!(text, ": {:?}", r.detail);
    }
    text
}

/// Builds the status report for `config` at `now`.
pub fn report(config: &Config, now: i64) -> StatusReport {
    let mode = match config.credentials {
        CredentialsMode::Static(_) => "static",
        CredentialsMode::Enrolled => "enrolled",
    };
    let daemon = RuntimeStatus::read(&config.status_path());
    let daemon_running = daemon.as_ref().is_some_and(|d| process_alive(d.pid));
    let (receivers, receivers_problem) = match (&daemon, daemon_running) {
        (Some(d), true) => (d.receivers.clone(), None),
        _ => match StoredSet::load(&config.receivers_path()) {
            Ok(set) => (applied(&set), None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        },
    };
    let mut r = StatusReport {
        credentials: mode,
        enrolled: false,
        problem: None,
        station_id: None,
        cert_not_after_unix_s: None,
        cert_expires_in_s: None,
        cert_serial: None,
        cert_renew_at_unix_s: None,
        hub_id: None,
        gateway_url: None,
        receivers,
        receivers_problem,
        daemon_running,
        daemon,
    };
    match credentials::load(config) {
        Ok(c) => {
            r.enrolled = true;
            r.station_id = Some(c.tls.station_id.to_string());
            r.cert_not_after_unix_s = Some(c.tls.validity.not_after_unix_s);
            r.cert_expires_in_s = Some(c.tls.validity.not_after_unix_s - now);
            r.cert_serial = Some(c.tls.serial);
            if let CredentialsMode::Enrolled = config.credentials {
                r.cert_renew_at_unix_s = Some(crate::renew::due_at(c.tls.validity));
            }
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
        if let Some(serial) = &self.cert_serial {
            line("serial", serial);
        }
        if let Some(at) = self.cert_renew_at_unix_s {
            line("renewal due", &format_utc(at));
        }
        if let Some(h) = &self.hub_id {
            line("home hub", h);
        }
        if let Some(g) = &self.gateway_url {
            line("gateway", g);
        }
        match (&self.receivers_problem, self.receivers.is_empty()) {
            (Some(p), _) => line("receivers", &format!("unusable: {p}")),
            (None, true) => line(
                "receivers",
                "none applied yet (set them up on the station's page in the web app)",
            ),
            (None, false) => {
                for r in &self.receivers {
                    line(
                        &format!("receiver {}", r.receiver_id),
                        &describe_receiver(r),
                    );
                }
            }
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
                    Tunnel::Revoked => {
                        "revoked: this station's certificate was revoked; enroll it again"
                            .to_owned()
                    }
                };
                let as_of = format_utc(d.updated_unix_s);
                if running {
                    line("daemon", &format!("running (pid {})", d.pid));
                } else {
                    line("daemon", &format!("not running (last status {as_of})"));
                }
                line("tunnel", &tunnel);
                line("backend", &d.backend_version);
                if let Some(e) = &d.last_error {
                    line("last error", &format!("{e:?}"));
                }
                if let Some(skew) = d.clock_skew_ms {
                    line("clock skew", &describe_skew(skew));
                }
                let r = &d.renewal;
                if let Some(at) = r.last_renewal_unix_s {
                    line("renewed", &format_utc(at));
                }
                if let Some(at) = r.next_renewal_unix_s {
                    line("next renewal", &format_utc(at));
                }
                if let Some(e) = &r.last_error {
                    line("renewal", &format!("failed: {e:?}"));
                }
            }
        }
        out
    }
}

/// The clock skew in words, with a warning above
/// [`CLOCK_SKEW_WARN_MS`].
pub fn describe_skew(skew_ms: i64) -> String {
    let secs = skew_ms.unsigned_abs() / 1000;
    let tenths = skew_ms.unsigned_abs() % 1000 / 100;
    let direction = if skew_ms >= 0 { "behind" } else { "ahead of" };
    let mut text = format!("this clock is {secs}.{tenths} s {direction} the hub's");
    if skew_ms.unsigned_abs() > CLOCK_SKEW_WARN_MS.unsigned_abs() {
        text.push_str(" (WARNING: more than 30 s; check NTP, certificates depend on the clock)");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_proto::v1;

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
        let receivers = Receivers::new(
            crate::receivers::ReceiverPaths {
                store: tmp.path().join("receivers.json"),
                usb_devices: tmp.path().join("usb"),
                packaged: false,
            },
            crate::receivers::Driver::fake(),
            crate::receivers::ReceiverOptions::default(),
        );
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
        let observed = Observed {
            gateway_url: "https://gw".into(),
            clock_skew_ms: Some(-1500),
            ..Observed::default()
        };
        for (state, tunnel) in states {
            let s = RuntimeStatus::new("id", &state, &observed, &receivers, Some("boom".into()));
            assert_eq!(s.tunnel, tunnel);
            assert_eq!(s.gateway_url, "https://gw");
            assert_eq!(s.clock_skew_ms, Some(-1500));
            assert!(s.backend_version.starts_with("fake-"));
            assert!(s.receivers.is_empty());
            s.write(&path).expect("write");
            assert_eq!(RuntimeStatus::read(&path), Some(s));
        }
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(u32::MAX));
        assert!(
            RuntimeStatus::new(
                "id",
                &ConnectionState::Connecting { attempt: 1 },
                &Observed::default(),
                &receivers,
                None
            )
            .write(&tmp.path().join("no/dir/status.json"))
            .is_err()
        );
    }

    #[test]
    fn receivers_in_words() {
        let mut r = ReceiverSummary {
            receiver_id: 11,
            state: "running".into(),
            detail: String::new(),
            profile: "rtlsdr-v4".into(),
            sdr_serial: "A0000011".into(),
            center_frequency_hz: 14_200_000,
            slot: 3,
        };
        assert_eq!(
            describe_receiver(&r),
            "running, rtlsdr-v4 at 14200000 Hz on SDR A0000011 (slot 3)"
        );
        r.state = "no_device".into();
        r.detail = "no SDR with serial A0000011 is plugged in".into();
        r.sdr_serial = String::new();
        let text = describe_receiver(&r);
        assert!(text.contains("on the only RTL-SDR"), "{text}");
        assert!(
            text.ends_with(": \"no SDR with serial A0000011 is plugged in\""),
            "{text}"
        );
    }

    #[test]
    fn skew_in_words() {
        assert_eq!(describe_skew(0), "this clock is 0.0 s behind the hub's");
        assert_eq!(
            describe_skew(-1250),
            "this clock is 1.2 s ahead of the hub's"
        );
        assert!(!describe_skew(30_000).contains("WARNING"));
        assert!(describe_skew(-30_001).contains("WARNING"));
        assert!(describe_skew(i64::MIN).contains("WARNING"));
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
            cert_serial: Some("abc".into()),
            cert_renew_at_unix_s: Some(1_789_000_000),
            hub_id: Some("sa1".into()),
            gateway_url: Some("https://gw".into()),
            receivers: Vec::new(),
            receivers_problem: None,
            daemon_running: false,
            daemon: None,
        };
        let text = base.render();
        assert!(text.contains("enrolled     yes\n"), "{text}");
        assert!(text.contains("in 20d 0h"), "{text}");
        assert!(text.contains("not running?"), "{text}");
        assert!(text.contains("receivers    none applied yet"), "{text}");
        let broken = StatusReport {
            receivers_problem: Some("invalid receivers.json".into()),
            ..base.clone()
        };
        assert!(broken.render().contains("receivers    unusable: invalid"));
        let one = StatusReport {
            receivers: vec![ReceiverSummary {
                receiver_id: 2,
                state: "applied".into(),
                detail: String::new(),
                profile: "rtlsdr".into(),
                sdr_serial: "S".into(),
                center_frequency_hz: 98_000_000,
                slot: 0,
            }],
            ..base.clone()
        };
        let text = one.render();
        assert!(
            text.contains("receiver 2   applied, rtlsdr at 98000000 Hz"),
            "{text}"
        );
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
                    backend_version: "fake-1".into(),
                    receivers: Vec::new(),
                    last_error: Some("x\ny".into()),
                    clock_skew_ms: Some(45_000),
                    renewal: RenewalStatus {
                        last_renewal_unix_s: Some(1_780_000_000),
                        next_renewal_unix_s: Some(1_789_000_000),
                        last_error: Some("down".into()),
                    },
                }),
                ..base.clone()
            };
            let text = r.render();
            assert!(text.contains("tunnel"), "{text}");
            assert!(text.contains("serial       abc"), "{text}");
            assert!(
                text.contains("clock skew   this clock is 45.0 s behind"),
                "{text}"
            );
            assert!(text.contains("WARNING"), "{text}");
            assert!(text.contains("renewed      2026-05-28"), "{text}");
            assert!(text.contains("next renewal"), "{text}");
            assert!(text.contains("renewal      failed: \"down\""), "{text}");
            assert!(text.contains("x\\ny"), "control characters escaped: {text}");
        }
    }
}
