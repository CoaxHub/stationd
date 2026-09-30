// SPDX-License-Identifier: GPL-3.0-only
//! `stationd doctor`: checks an installation and prints one line per check.
//!
//! | Check | Fails when |
//! |---|---|
//! | `config` | `stationd.toml` is invalid (nothing else runs) |
//! | `enrollment` | the credentials are missing or unusable |
//! | `sdr` | no USB device of the profile (RTL-SDR `0bda:2838`/`0bda:2832`) |
//! | `radiod` | radiod's status group does not answer within 3 s |
//! | `enroll` | DNS or the TLS handshake with the enrollment service fails, against the `CoaxHub` root (a warning once enrolled) |
//! | `station-api` | the mTLS handshake with `StationControl` fails, against the gateway CA bundle (a warning: the station keeps running on its stored assignment, and renewal retries) |
//! | `gateway` | the mTLS handshake with the home hub's gateway fails, against the gateway CA bundle |
//! | `clock` | the clock is before the certificate's `notBefore`, or the certificate expired (a warning once two thirds of its lifetime passed) |
//! | `clock-skew` | never: a warning when the daemon measured more than 30 s between this clock and the hub's |

use std::fmt::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend_ka9q::client::{ClientConfig, Ka9qClient};

use crate::config::{BackendConfig, Config, Endpoint};
use crate::credentials::{self, Credentials};
use crate::enroll;
use crate::local_status::{self, RuntimeStatus, describe_skew, format_duration, format_utc};
use crate::profile::UsbId;
use crate::tls::{self, Connector};

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Not applicable here.
    Skip,
    /// Fine.
    Pass,
    /// Works, but needs attention.
    Warn,
    /// Broken.
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Skip => "skip",
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        })
    }
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: &'static str,
    /// The outcome.
    pub level: Level,
    /// Details for the owner.
    pub detail: String,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<4}  {:<11}  {}", self.level, self.name, self.detail)
    }
}

fn check(name: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
    }
}

/// Where the checks look, and how long they wait.
#[derive(Debug, Clone)]
pub struct Environment {
    /// `/sys/bus/usb/devices`.
    pub usb_devices: PathBuf,
    /// Now, Unix seconds.
    pub now_unix_s: i64,
    /// Per network check.
    pub timeout: Duration,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            usb_devices: PathBuf::from("/sys/bus/usb/devices"),
            now_unix_s: crate::local_status::unix_now(),
            timeout: Duration::from_secs(3),
        }
    }
}

/// Runs every check for the config at `path`.
pub async fn run(path: &Path, env: &Environment) -> Vec<Check> {
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => return vec![check("config", Level::Fail, e.to_string())],
    };
    run_config(&config, path, env).await
}

/// Runs every check for a loaded `config`.
pub async fn run_config(config: &Config, path: &Path, env: &Environment) -> Vec<Check> {
    let mut out = vec![check(
        "config",
        Level::Pass,
        format!(
            "{} is valid ({} build)",
            path.display(),
            config.flavour.name()
        ),
    )];
    let creds = credentials::load(config);
    out.push(match &creds {
        Ok(c) => check(
            "enrollment",
            Level::Pass,
            format!(
                "station {} ({} credentials)",
                c.tls.station_id,
                credentials::source(config).kind()
            ),
        ),
        Err(e) => check("enrollment", Level::Fail, e.to_string()),
    });
    let creds = creds.ok();
    out.push(sdr(config, &env.usb_devices));
    out.push(radiod(config, env.timeout).await);
    out.push(enroll_endpoint(config, creds.is_some(), env.timeout).await);
    out.extend(mtls_checks(config, creds.as_ref(), env.timeout).await);
    out.push(clock(creds.as_ref(), env.now_unix_s));
    out.push(clock_skew(
        RuntimeStatus::read(&config.status_path()).as_ref(),
    ));
    out
}

/// The skew the daemon measured against the hub (`status.json`): a
/// warning above 30 s, never a failure.
fn clock_skew(status: Option<&RuntimeStatus>) -> Check {
    let Some((skew, at)) = status.and_then(|s| s.clock_skew_ms.map(|k| (k, s.updated_unix_s)))
    else {
        return check(
            "clock-skew",
            Level::Skip,
            "not measured yet (the daemon measures it when it connects to the hub)",
        );
    };
    let level = if skew.unsigned_abs() > local_status::CLOCK_SKEW_WARN_MS.unsigned_abs() {
        Level::Warn
    } else {
        Level::Pass
    };
    check(
        "clock-skew",
        level,
        format!("{} (as of {})", describe_skew(skew), format_utc(at)),
    )
}

/// Whether any check failed.
pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}

/// USB devices present under `devices` (`idVendor`/`idProduct` files).
pub fn usb_devices(devices: &Path) -> Vec<(String, UsbId)> {
    let Ok(entries) = std::fs::read_dir(devices) else {
        return Vec::new();
    };
    let hex = |p: PathBuf| -> Option<u16> {
        u16::from_str_radix(std::fs::read_to_string(p).ok()?.trim(), 16).ok()
    };
    let mut found: Vec<(String, UsbId)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let dir = e.path();
            let id = UsbId {
                vendor: hex(dir.join("idVendor"))?,
                product: hex(dir.join("idProduct"))?,
            };
            Some((e.file_name().to_string_lossy().into_owned(), id))
        })
        .collect();
    found.sort();
    found
}

fn sdr(config: &Config, devices: &Path) -> Check {
    let Some(sdr) = config.sdr else {
        return check(
            "sdr",
            Level::Skip,
            format!("no [sdr] profile ({} backend)", config.backend.kind()),
        );
    };
    let p = sdr.profile;
    if p.usb_ids.is_empty() {
        return check(
            "sdr",
            Level::Pass,
            format!("profile {} needs no hardware", p.name),
        );
    }
    let found: Vec<String> = usb_devices(devices)
        .into_iter()
        .filter(|(_, id)| p.usb_ids.contains(id))
        .map(|(port, id)| format!("{id} at {port}"))
        .collect();
    if found.is_empty() {
        let ids: Vec<String> = p.usb_ids.iter().map(ToString::to_string).collect();
        check(
            "sdr",
            Level::Fail,
            format!(
                "no {} found on USB (looked for {}); plug it in, or change [sdr] profile",
                p.description,
                ids.join(", ")
            ),
        )
    } else {
        check(
            "sdr",
            Level::Pass,
            format!("profile {}: {}", p.name, found.join(", ")),
        )
    }
}

/// Probes radiod's status group once, within `timeout`.
pub async fn probe_radiod(status_group: SocketAddr, timeout: Duration) -> Result<String, String> {
    let client = Ka9qClient::connect(ClientConfig {
        status_addr: status_group,
        // An unused local port: the probe needs no RTP.
        data_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        channel_status_addr: None,
        ..ClientConfig::default()
    })
    .map_err(|e| format!("cannot open radiod's status group {status_group}: {e}"))?;
    let started = Instant::now();
    let status = tokio::time::timeout(timeout, client.probe())
        .await
        .map_err(|_| {
            format!(
                "no answer on {status_group} within {} s (is coaxhub-radiod running?)",
                timeout.as_secs()
            )
        })?
        .map_err(|e| format!("radiod on {status_group}: {e}"))?;
    let mut detail = format!(
        "answered on {status_group} in {} ms",
        started.elapsed().as_millis()
    );
    if let Some(d) = &status.description {
        let _ = write!(detail, ", front end {d:?}");
    }
    if let Some(rate) = status.input_samprate {
        let _ = write!(detail, ", {rate} S/s");
    }
    Ok(detail)
}

async fn radiod(config: &Config, timeout: Duration) -> Check {
    match config.backend {
        BackendConfig::Fake => check("radiod", Level::Skip, "fake backend (development)"),
        BackendConfig::Ka9q(k) => match probe_radiod(k.status_group, timeout).await {
            Ok(d) => check("radiod", Level::Pass, d),
            Err(e) => check("radiod", Level::Fail, e),
        },
    }
}

/// DNS, then a TLS 1.3 handshake with ALPN h2, within `timeout`.
pub async fn handshake(
    endpoint: &Endpoint,
    config: Arc<rustls::ClientConfig>,
    timeout: Duration,
) -> Result<String, String> {
    let authority = endpoint.authority();
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(timeout, tokio::net::lookup_host(authority.as_str()))
            .await
            .map_err(|_| format!("DNS lookup of {} timed out", endpoint.host))?
            .map_err(|e| format!("DNS lookup of {} failed: {e}", endpoint.host))?
            .collect();
    let first = addrs
        .first()
        .ok_or_else(|| format!("DNS lookup of {} returned no address", endpoint.host))?;
    let connector = Connector::new(config, endpoint).map_err(|e| e.to_string())?;
    tokio::time::timeout(timeout, connector.connect())
        .await
        .map_err(|_| {
            format!(
                "{authority}: no TLS handshake within {} s",
                timeout.as_secs()
            )
        })?
        .map_err(|e| e.to_string())?;
    Ok(format!("{authority} ({first}): TLS 1.3 + h2 OK"))
}

async fn enroll_endpoint(config: &Config, enrolled: bool, timeout: Duration) -> Check {
    let Some(endpoints) = &config.endpoints else {
        return check(
            "enroll",
            Level::Skip,
            "no enrollment endpoint (development build without [endpoints])",
        );
    };
    let failure = if enrolled { Level::Warn } else { Level::Fail };
    let roots = match enroll::trust_anchor_roots(&endpoints.root) {
        Ok(r) => r,
        Err(e) => return check("enroll", Level::Fail, e.to_string()),
    };
    let tls_config = match tls::client_config(roots, None) {
        Ok(c) => Arc::new(c),
        Err(e) => return check("enroll", Level::Fail, e.to_string()),
    };
    match handshake(&endpoints.enroll, tls_config, timeout).await {
        Ok(d) => check(
            "enroll",
            Level::Pass,
            format!("{d}, verified against {}", endpoints.root),
        ),
        Err(e) => check("enroll", failure, e),
    }
}

async fn mtls_checks(
    config: &Config,
    creds: Option<&Credentials>,
    timeout: Duration,
) -> Vec<Check> {
    let Some(c) = creds else {
        return vec![
            check("station-api", Level::Skip, "not enrolled"),
            check("gateway", Level::Skip, "not enrolled"),
        ];
    };
    let mut out = Vec::new();
    match &config.endpoints {
        None => out.push(check(
            "station-api",
            Level::Skip,
            "no station-api endpoint (development build without [endpoints])",
        )),
        Some(e) => out.push(
            match handshake(&e.station_api, Arc::clone(&c.tls.config), timeout).await {
                Ok(d) => check("station-api", Level::Pass, d),
                // The stored assignment keeps working; renewal retries.
                Err(err) => check(
                    "station-api",
                    Level::Warn,
                    format!("{err} (needed for certificate renewal and hub assignment)"),
                ),
            },
        ),
    }
    out.push(
        match handshake(&c.gateway, Arc::clone(&c.tls.config), timeout).await {
            Ok(d) => check(
                "gateway",
                Level::Pass,
                format!("hub {}: {d}", c.hub_id.as_deref().unwrap_or("(static)")),
            ),
            Err(err) => check("gateway", Level::Fail, err),
        },
    );
    out
}

fn clock(creds: Option<&Credentials>, now: i64) -> Check {
    let Some(c) = creds else {
        return check("clock", Level::Skip, "no certificate to compare with");
    };
    let v = c.tls.validity;
    if now < v.not_before_unix_s {
        return check(
            "clock",
            Level::Fail,
            format!(
                "the system clock is {} behind the certificate's start; fix the time (NTP)",
                format_duration(v.not_before_unix_s - now)
            ),
        );
    }
    if now >= v.not_after_unix_s {
        return check(
            "clock",
            Level::Fail,
            format!(
                "the certificate expired {} ago; enroll this station again",
                format_duration(now - v.not_after_unix_s)
            ),
        );
    }
    let lifetime = v.not_after_unix_s - v.not_before_unix_s;
    let left = v.not_after_unix_s - now;
    if left * 3 < lifetime {
        return check(
            "clock",
            Level::Warn,
            format!(
                "the certificate expires in {}; it should have been renewed",
                format_duration(left)
            ),
        );
    }
    check(
        "clock",
        Level::Pass,
        format!("certificate valid for another {}", format_duration(left)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{ClientTls, Validity};

    fn usb(dir: &Path, port: &str, vendor: &str, product: &str) {
        let d = dir.join(port);
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join("idVendor"), format!("{vendor}\n")).expect("write");
        std::fs::write(d.join("idProduct"), format!("{product}\n")).expect("write");
    }

    #[test]
    fn usb_scan() {
        let tmp = tempfile::tempdir().expect("tmp");
        usb(tmp.path(), "1-1", "1d6b", "0002");
        usb(tmp.path(), "1-1.3", "0bda", "2838");
        std::fs::create_dir(tmp.path().join("usb1")).expect("hub without ids");
        usb(tmp.path(), "2-1", "zz", "0002");
        let found = usb_devices(tmp.path());
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].0, "1-1.3");
        assert!(usb_devices(&tmp.path().join("none")).is_empty());
    }

    fn creds(not_before: i64, not_after: i64) -> Credentials {
        let pki = station_conformance::pki::TestPki::generate().expect("pki");
        let tmp = tempfile::tempdir().expect("tmp");
        let w = |n: &str, c: &str| {
            let p = tmp.path().join(n);
            std::fs::write(&p, c).expect("write");
            p
        };
        let (cert, key, ca) = (
            w("c", &pki.station_cert_pem),
            w("k", &pki.station_key_pem),
            w("ca", &pki.ca_pem),
        );
        let tls = tls::load_files(tls::CredentialFiles {
            cert: &cert,
            chain: None,
            key: &key,
            ca: &ca,
        })
        .expect("tls");
        Credentials {
            gateway: Endpoint::parse("x", "https://localhost:1").expect("url"),
            hub_id: None,
            tls: ClientTls {
                validity: Validity {
                    not_before_unix_s: not_before,
                    not_after_unix_s: not_after,
                },
                ..tls
            },
        }
    }

    #[test]
    fn clock_checks() {
        let c = creds(1000, 1000 + 30 * 86_400);
        assert_eq!(clock(None, 0).level, Level::Skip);
        let behind = clock(Some(&c), 400);
        assert_eq!(behind.level, Level::Fail);
        assert!(behind.detail.contains("10m 0s behind"), "{}", behind.detail);
        assert_eq!(clock(Some(&c), 1000 + 86_400).level, Level::Pass);
        let due = clock(Some(&c), 1000 + 25 * 86_400);
        assert_eq!(due.level, Level::Warn, "{}", due.detail);
        let expired = clock(Some(&c), 1000 + 31 * 86_400);
        assert_eq!(expired.level, Level::Fail);
        assert!(expired.detail.contains("enroll"), "{}", expired.detail);
    }

    #[test]
    fn clock_skew_checks() {
        assert_eq!(clock_skew(None).level, Level::Skip);
        let status = |skew| RuntimeStatus {
            pid: 1,
            updated_unix_s: 0,
            station_id: "s".into(),
            gateway_url: "g".into(),
            tunnel: local_status::Tunnel::Connected,
            connection_id: None,
            backoff_ms: None,
            backend: "running".into(),
            backend_version: "v".into(),
            last_error: None,
            clock_skew_ms: skew,
            renewal: crate::renew::RenewalStatus::default(),
        };
        assert_eq!(clock_skew(Some(&status(None))).level, Level::Skip);
        let ok = clock_skew(Some(&status(Some(-2000))));
        assert_eq!(ok.level, Level::Pass);
        assert!(ok.detail.contains("2.0 s ahead"), "{}", ok.detail);
        let off = clock_skew(Some(&status(Some(90_000))));
        assert_eq!(off.level, Level::Warn, "never a failure");
        assert!(off.detail.contains("90.0 s behind"), "{}", off.detail);
        assert!(off.detail.contains("1970-01-01"), "{}", off.detail);
    }

    #[test]
    fn levels_and_lines() {
        let c = check("sdr", Level::Fail, "gone");
        assert_eq!(c.to_string(), "FAIL  sdr          gone");
        assert!(failed(&[check("a", Level::Pass, ""), c.clone()]));
        assert!(!failed(&[
            check("a", Level::Warn, ""),
            check("b", Level::Skip, "")
        ]));
        assert!(Level::Fail > Level::Warn);
        for l in [Level::Skip, Level::Pass, Level::Warn] {
            assert_eq!(l.to_string().len(), 4);
        }
    }

    #[tokio::test]
    async fn handshake_errors() {
        let pki = station_conformance::pki::TestPki::generate().expect("pki");
        let roots = tls::root_store(tls::parse_certs(&pki.ca_pem).expect("ca")).expect("roots");
        let config = Arc::new(tls::client_config(roots, None).expect("config"));
        let bad = Endpoint::parse("x", "https://no-such-host.invalid:443").expect("url");
        let e = handshake(&bad, Arc::clone(&config), Duration::from_secs(3))
            .await
            .expect_err("dns");
        assert!(e.contains("DNS"), "{e}");
        // Nothing listening.
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        let closed = Endpoint::parse("x", &format!("https://127.0.0.1:{port}")).expect("url");
        let e = handshake(&closed, config, Duration::from_secs(3))
            .await
            .expect_err("closed");
        assert!(e.contains("TCP connect"), "{e}");
    }
}
