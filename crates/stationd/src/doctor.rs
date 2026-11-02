// SPDX-License-Identifier: GPL-3.0-only
//! `stationd doctor`: checks an installation and prints one line per check,
//! so a station without its tunnel still explains itself.
//!
//! | Check | Fails when |
//! |---|---|
//! | `config` | a development build's `stationd.toml` is invalid (nothing else runs) |
//! | `enrollment` | the credentials are missing or unusable |
//! | `sdrs` | never: the SDRs it sees, a warning when there is no RTL-SDR (a station runs without one until it arrives) |
//! | `receivers` | `receivers.json` is unusable; a warning per receiver whose SDR is missing |
//! | `sdr-holders` | never: per RTL-SDR, the programs that have its USB device open (a warning when one is not this station's own radiod and a receiver is set to use that SDR, or when a receiver on it failed and the look was partial; a program on an SDR no receiver uses is only reported), and the kernel driver bound to it; it says plainly when it could not inspect other users' processes (run doctor with `sudo`) |
//! | `helper` | `coaxhub-helper` (the root helper that starts the receivers' units) does not answer `ping` on its socket (a warning when this user may not use the socket: run doctor with `sudo`); skipped for the fake and external backends |
//! | `radiod` | a receiver's radiod does not answer on its slot's status group within 3 s |
//! | `enroll` | DNS or the TLS handshake with the enrollment service fails, against the `CoaxHub` root (a warning once enrolled) |
//! | `station-api` | the mTLS handshake with `StationControl` fails, against the gateway CA bundle (a warning: the station keeps running on its stored assignment, and renewal retries) |
//! | `gateway` | the mTLS handshake with the home hub's gateway fails, against the gateway CA bundle |
//! | `clock` | the clock is before the certificate's `notBefore`, or the certificate expired (a warning once two thirds of its lifetime passed) |
//! | `clock-skew` | never: a warning when the daemon measured more than 30 s between this clock and the hub's |

use std::fmt::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use backend_ka9q::client::{ClientConfig, Ka9qClient};

use crate::cli::ConfigArg;
use crate::config::{BackendConfig, Config, Endpoint};
use crate::credentials::{self, Credentials};
use crate::enroll;
use crate::hardware::{self, Hardware};
use crate::local_status::{
    self, RuntimeStatus, describe_receiver, describe_skew, format_duration, format_utc,
};
use crate::receivers::{CallError, Helper, StoredSet};
use crate::tls::{self, Connector};
use crate::usb_holders;

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
    /// The USB devices; `None`: the configuration's
    /// (`/sys/bus/usb/devices`).
    pub usb_devices: Option<PathBuf>,
    /// The root helper's socket; `None`: `/run/coaxhub-helper.sock`.
    pub helper_socket: Option<PathBuf>,
    /// The processes (`/proc`); `None`: `/proc`.
    pub proc_dir: Option<PathBuf>,
    /// The USB device nodes; `None`: `/dev/bus/usb`.
    pub dev_bus_usb: Option<PathBuf>,
    /// Now, Unix seconds.
    pub now_unix_s: i64,
    /// Per network check.
    pub timeout: Duration,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            usb_devices: None,
            helper_socket: None,
            proc_dir: None,
            dev_bus_usb: None,
            now_unix_s: crate::local_status::unix_now(),
            timeout: Duration::from_secs(3),
        }
    }
}

/// Runs every check for this build's settings.
pub async fn run(arg: &ConfigArg, env: &Environment) -> Vec<Check> {
    let config = match arg.load() {
        Ok(c) => c,
        Err(e) => return vec![check("config", Level::Fail, e.to_string())],
    };
    run_config(&config, &arg.describe(), env).await
}

/// Runs every check for a loaded `config` (from `source`).
pub async fn run_config(config: &Config, source: &str, env: &Environment) -> Vec<Check> {
    let detail = if config.flavour.packaged {
        "packaged build: built-in settings, no settings file".to_owned()
    } else {
        format!("{source} is valid ({} build)", config.flavour.name())
    };
    let mut out = vec![check("config", Level::Pass, detail)];
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
    let usb = env.usb_devices.as_deref().unwrap_or(&config.usb_devices);
    let hw = Hardware::scan(usb);
    out.push(sdrs(&hw));
    let daemon =
        RuntimeStatus::read(&config.status_path()).filter(|d| local_status::process_alive(d.pid));
    let applied = match StoredSet::load(&config.receivers_path()) {
        Ok(set) => {
            out.extend(receivers(&set, &hw, daemon.as_ref()));
            out.extend(radiods(config, &set, env.timeout).await);
            set
        }
        Err(e) => {
            out.push(check("receivers", Level::Fail, e.to_string()));
            StoredSet::default()
        }
    };
    out.extend(sdr_holders(&hw, usb, env, &applied, daemon.as_ref()));
    out.push(helper(&config.backend, env).await);
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

/// The SDRs plugged in: a warning without an RTL-SDR, never a failure (a
/// station runs without one until it arrives).
fn sdrs(hw: &Hardware) -> Check {
    let lines = hw.describe();
    if hw.devices.iter().any(hardware::UsbDevice::is_rtlsdr) {
        check("sdrs", Level::Pass, lines.join("; "))
    } else {
        let ids: Vec<String> = hardware::RTLSDR_IDS
            .iter()
            .map(|(v, p)| format!("{v:04x}:{p:04x}"))
            .collect();
        let mut detail = format!(
            "no RTL-SDR on USB (looked for {}); plug one in, then set the receivers up on \
             the station's page",
            ids.join(", ")
        );
        if !lines.is_empty() {
            let _ = write!(detail, "; seen: {}", lines.join("; "));
        }
        check("sdrs", Level::Warn, detail)
    }
}

/// One line per applied receiver: its state from the running daemon, else
/// whether its SDR is here.
fn receivers(set: &StoredSet, hw: &Hardware, daemon: Option<&RuntimeStatus>) -> Vec<Check> {
    if set.receivers.is_empty() {
        return vec![check(
            "receivers",
            Level::Pass,
            "none applied yet: the hub sends them once the owner sets them up on the \
             station's page",
        )];
    }
    set.receivers
        .iter()
        .map(|r| {
            let running =
                daemon.and_then(|d| d.receivers.iter().find(|s| s.receiver_id == r.receiver_id));
            let here = r.profile.profile().dev_only
                || hardware::device_for(&hw.devices, &r.sdr_serial).is_some();
            let summary = running.cloned().unwrap_or_else(|| {
                local_status::applied(&StoredSet {
                    receivers: vec![r.clone()],
                })
                .remove(0)
            });
            let mut detail = format!(
                "receiver {}: {}",
                r.receiver_id,
                describe_receiver(&summary)
            );
            let level = match (summary.state.as_str(), here) {
                ("failed" | "no_device", _) | (_, false) => Level::Warn,
                _ => Level::Pass,
            };
            if !here {
                detail.push_str("; its SDR is not plugged in");
            }
            check("receivers", level, detail)
        })
        .collect()
}

/// One line per RTL-SDR plugged in: the programs that have its USB device
/// open, and the kernel driver bound to it. An SDR serves one program at a
/// time, so a receiver stuck `failed` (or `starting`) on an SDR another
/// program holds is told here. Linux shows the open files of the caller's
/// own processes only (all of them to root), so when other users' processes
/// were out of sight it says so, and a look that found nothing is not
/// "nobody has it".
fn sdr_holders(
    hw: &Hardware,
    usb: &std::path::Path,
    env: &Environment,
    set: &StoredSet,
    daemon: Option<&RuntimeStatus>,
) -> Vec<Check> {
    let proc_dir = env
        .proc_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(usb_holders::PROC_DIR));
    let dev_root = env
        .dev_bus_usb
        .clone()
        .unwrap_or_else(|| PathBuf::from(usb_holders::DEV_BUS_USB));
    hw.devices
        .iter()
        .filter(|d| d.is_rtlsdr())
        .map(|dev| {
            let label = if dev.serial.is_empty() {
                format!("the RTL-SDR at USB port {}", dev.port)
            } else {
                format!("SDR {} (USB port {})", dev.serial, dev.port)
            };
            let stuck = daemon.is_some_and(|d| {
                d.receivers.iter().any(|r| {
                    r.state == "failed" && (r.sdr_serial.is_empty() || r.sdr_serial == dev.serial)
                })
            });
            // Whether a receiver is set to use this SDR: a program holding
            // one that none uses (an `rtl_tcp` run on purpose on a second
            // dongle) is information, not a problem.
            let used = set.receivers.iter().any(|r| {
                hardware::device_for(&hw.devices, &r.sdr_serial).is_some_and(|d| d.port == dev.port)
            });
            let mut level = Level::Pass;
            let mut detail = label;
            if let Some(node) = usb_holders::device_node(usb, &dev.port, &dev_root) {
                let found = usb_holders::holders(&proc_dir, &node);
                let foreign: Vec<String> = found
                    .foreign()
                    .map(|h| format!("{} (process {})", h.name, h.pid))
                    .collect();
                let own: Vec<String> = found
                    .holders
                    .iter()
                    .filter_map(|h| h.receiver)
                    .map(|id| format!("receiver {id}"))
                    .collect();
                if !foreign.is_empty() && used {
                    level = Level::Warn;
                    let _ = write!(
                        detail,
                        ": in use by another program on this computer: {}; close it, and the \
                         receiver starts again by itself",
                        foreign.join(", ")
                    );
                } else if !foreign.is_empty() {
                    let _ = write!(
                        detail,
                        ": in use by {}; no receiver of this station is set to use this SDR",
                        foreign.join(", ")
                    );
                } else if !own.is_empty() {
                    let _ = write!(detail, ": used by this station's {}", own.join(", "));
                } else if found.uninspected > 0 {
                    // Nothing seen, but not everything could be.
                    if stuck {
                        level = Level::Warn;
                    }
                    let _ = write!(
                        detail,
                        ": no program doctor can see has it open, but {} processes of other \
                         users could not be inspected; run `sudo stationd doctor` to see them all",
                        found.uninspected
                    );
                } else {
                    detail.push_str(": no program has it open");
                }
            } else {
                detail.push_str(
                    ": cannot tell which program has it open (the system does not give its \
                     USB device number)",
                );
            }
            if let Some(driver) = usb_holders::bound_driver(usb, &dev.port) {
                if stuck {
                    level = level.max(Level::Warn);
                }
                let _ = write!(detail, "; the kernel driver {driver} is bound to it");
                if driver == "dvb_usb_rtl28xxu" {
                    // The DVB-T driver holds an RTL-SDR; the package
                    // blacklists it for later boots.
                    let _ = write!(
                        detail,
                        " (it holds the SDR until `sudo rmmod {driver}` or a reboot; the \
                         package blacklists it)"
                    );
                }
            }
            check("sdr-holders", level, detail)
        })
        .collect()
}

/// Whether `coaxhub-helper` answers `ping`: the door `stationd` starts the
/// receivers' units through (packaging.md §4).
async fn helper(backend: &BackendConfig, env: &Environment) -> Check {
    if !matches!(backend, BackendConfig::Bundled) {
        return check(
            "helper",
            Level::Skip,
            "fake or external backend (development)",
        );
    }
    let units = env
        .helper_socket
        .as_deref()
        .map_or_else(Helper::default, Helper::at);
    let socket = units.socket.display().to_string();
    let answer = tokio::time::timeout(env.timeout, units.ping())
        .await
        .unwrap_or_else(|_| {
            Err(CallError {
                text: "coaxhub-helper timed out".to_owned(),
                denied: false,
            })
        });
    let Err(why) = answer else {
        return check("helper", Level::Pass, format!("answers on {socket}"));
    };
    let denied = why.denied;
    let why = why.text;
    let why = why.trim_end_matches("; run sudo stationd doctor");
    if denied {
        return check(
            "helper",
            Level::Warn,
            format!(
                "this user may not use {socket} (only root and the coaxhub user may); run \
                 `sudo stationd doctor`"
            ),
        );
    }
    let mut detail = format!("the helper does not answer: {why}");
    if env.helper_socket.is_none() {
        if let Ok(state) = units
            .systemd
            .property("coaxhub-helper.socket", "ActiveState")
            .await
        {
            let _ = write!(detail, "; coaxhub-helper.socket is {state}");
        }
    }
    detail.push_str("; without it no receiver can start. Fix: sudo systemctl restart stationd");
    check("helper", Level::Fail, detail)
}

/// Probes each receiver's radiod.
async fn radiods(config: &Config, set: &StoredSet, timeout: Duration) -> Vec<Check> {
    match config.backend {
        BackendConfig::Fake => vec![check("radiod", Level::Skip, "fake backend (development)")],
        _ if set.receivers.is_empty() => {
            vec![check("radiod", Level::Skip, "no receiver, no radiod")]
        }
        BackendConfig::External(k) => vec![match probe_radiod(k.status_group, timeout).await {
            Ok(d) => check("radiod", Level::Pass, d),
            Err(e) => check("radiod", Level::Fail, e),
        }],
        BackendConfig::Bundled => {
            let mut out = Vec::new();
            for r in &set.receivers {
                let (status, _) = r.groups();
                out.push(match probe_radiod(status, timeout).await {
                    Ok(d) => check(
                        "radiod",
                        Level::Pass,
                        format!("receiver {}: {d}", r.receiver_id),
                    ),
                    Err(e) => check(
                        "radiod",
                        Level::Fail,
                        format!("receiver {}: {e}", r.receiver_id),
                    ),
                });
            }
            out
        }
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
                "no answer on {status_group} within {} s (is its coaxhub-radiod@ unit running?)",
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

    fn usb(dir: &std::path::Path, port: &str, ids: (&str, &str), product: &str, serial: &str) {
        let d = dir.join(port);
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join("idVendor"), format!("{}\n", ids.0)).expect("write");
        std::fs::write(d.join("idProduct"), format!("{}\n", ids.1)).expect("write");
        std::fs::write(d.join("product"), format!("{product}\n")).expect("write");
        std::fs::write(d.join("serial"), format!("{serial}\n")).expect("write");
    }

    #[test]
    fn sdrs_and_receivers() {
        let tmp = tempfile::tempdir().expect("tmp");
        usb(tmp.path(), "usb1", ("1d6b", "0002"), "hub", "");
        let none = sdrs(&Hardware::scan(tmp.path()));
        assert_eq!(none.level, Level::Warn);
        assert!(
            none.detail.to_lowercase().contains("no rtl-sdr"),
            "{}",
            none.detail
        );
        usb(tmp.path(), "1-1.3", ("0bda", "2838"), "Blog V4", "A1");
        let hw = Hardware::scan(tmp.path());
        let some = sdrs(&hw);
        assert_eq!(some.level, Level::Pass);
        assert!(some.detail.contains("RTL-SDR Blog V4"), "{}", some.detail);
        let empty = receivers(&StoredSet::default(), &hw, None);
        assert_eq!(empty[0].level, Level::Pass);
        assert!(empty[0].detail.contains("none applied yet"));
        let set: StoredSet = serde_json::from_str(
            r#"{"receivers":[
                {"receiver_id":1,"slot":0,"sdr_serial":"A1","model":"rtlsdr_blog_v4","profile":"rtlsdr_v4","center_frequency_hz":7100000},
                {"receiver_id":2,"slot":1,"sdr_serial":"B2","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":98000000}]}"#,
        )
        .expect("set");
        let lines = receivers(&set, &hw, None);
        assert_eq!(lines[0].level, Level::Pass, "{}", lines[0].detail);
        assert!(
            lines[0]
                .detail
                .starts_with("receiver 1: applied, rtlsdr-v4"),
            "{}",
            lines[0].detail
        );
        assert_eq!(lines[1].level, Level::Warn);
        assert!(
            lines[1].detail.ends_with("its SDR is not plugged in"),
            "{}",
            lines[1].detail
        );
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
            backend_version: "v".into(),
            receivers: Vec::new(),
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

    #[tokio::test]
    async fn helper_checks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let socket = tmp.path().join("helper.sock");
        let env = Environment {
            helper_socket: Some(socket.clone()),
            timeout: Duration::from_millis(500),
            ..Environment::default()
        };
        let config = BackendConfig::Bundled;
        // Not a bundled station: nothing to check.
        assert_eq!(helper(&BackendConfig::Fake, &env).await.level, Level::Skip);
        // No socket: the fix is named.
        let c = helper(&config, &env).await;
        assert_eq!(c.level, Level::Fail, "{c}");
        assert!(c.detail.contains("does not answer"), "{c}");
        assert!(c.detail.contains("helper.sock"), "{c}");
        assert!(c.detail.contains("sudo systemctl restart stationd"), "{c}");
        assert!(!c.detail.contains("run sudo stationd doctor"), "{c}");
        // A helper that answers ping.
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            use std::io::{BufRead as _, BufReader, Write as _};
            for stream in listener.incoming().take(1) {
                let mut stream = stream.expect("accept");
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).expect("line");
                assert_eq!(line, "ping\n");
                if stream.write_all(b"ok\n").is_err() {
                    return;
                }
            }
        });
        let c = helper(&config, &env).await;
        assert_eq!(c.level, Level::Pass, "{c}");
        assert!(c.detail.contains("helper.sock"), "{c}");
        server.join().expect("server");
    }

    #[cfg(unix)]
    #[test]
    fn sdr_holders_name_the_program_and_the_driver() {
        let tmp = tempfile::tempdir().expect("tmp");
        let usb_dir = tmp.path().join("usb");
        usb(&usb_dir, "1-1.3", ("0bda", "2838"), "RTL2838UHIDIR", "A1");
        let env = Environment {
            proc_dir: Some(tmp.path().join("proc")),
            dev_bus_usb: Some(PathBuf::from("/dev/bus/usb")),
            ..Environment::default()
        };
        let hw = Hardware::scan(&usb_dir);
        // A receiver set to use the SDR.
        let set: StoredSet = serde_json::from_str(
            r#"{"receivers":[
                {"receiver_id":1,"slot":0,"sdr_serial":"A1","model":"rtlsdr_other","profile":"rtlsdr","center_frequency_hz":98000000}]}"#,
        )
        .expect("set");
        let proc_dir = tmp.path().join("proc");
        std::fs::create_dir_all(&proc_dir).expect("mkdir");
        // Without a bus number the node is not known.
        let c = &sdr_holders(&hw, &usb_dir, &env, &set, None)[0];
        assert_eq!(c.level, Level::Pass, "{c}");
        assert!(c.detail.contains("cannot tell which program"), "{c}");
        std::fs::write(usb_dir.join("1-1.3/busnum"), "1\n").expect("write");
        std::fs::write(usb_dir.join("1-1.3/devnum"), "7\n").expect("write");
        let c = &sdr_holders(&hw, &usb_dir, &env, &set, None)[0];
        assert_eq!(c.level, Level::Pass, "{c}");
        assert!(c.detail.ends_with("no program has it open"), "{c}");
        // rtl_tcp has it open.
        let fds = proc_dir.join("4242/fd");
        std::fs::create_dir_all(&fds).expect("mkdir");
        std::fs::write(proc_dir.join("4242/comm"), "rtl_tcp\n").expect("write");
        std::os::unix::fs::symlink("/dev/bus/usb/001/007", fds.join("3")).expect("link");
        let c = &sdr_holders(&hw, &usb_dir, &env, &set, None)[0];
        assert_eq!(c.level, Level::Warn, "{c}");
        assert!(
            c.detail
                .contains("in use by another program on this computer: rtl_tcp (process 4242)"),
            "{c}"
        );
        assert_eq!(c.name, "sdr-holders");
        // The same program on an SDR no receiver uses is information.
        let c = &sdr_holders(&hw, &usb_dir, &env, &StoredSet::default(), None)[0];
        assert_eq!(c.level, Level::Pass, "{c}");
        assert!(
            c.detail
                .contains("in use by rtl_tcp (process 4242); no receiver of this station"),
            "{c}"
        );
        // The DVB-T driver is bound to it.
        let interface = usb_dir.join("1-1.3/1-1.3:1.0");
        std::fs::create_dir_all(&interface).expect("mkdir");
        std::os::unix::fs::symlink(
            "../../../bus/usb/drivers/dvb_usb_rtl28xxu",
            interface.join("driver"),
        )
        .expect("link");
        let c = &sdr_holders(&hw, &usb_dir, &env, &set, None)[0];
        assert!(
            c.detail.contains("kernel driver dvb_usb_rtl28xxu is bound"),
            "{c}"
        );
        assert!(c.detail.contains("rmmod"), "{c}");
        // An SDR that is not an RTL-SDR gets no line.
        usb(&usb_dir, "1-1.4", ("1d50", "6089"), "HackRF", "H1");
        assert_eq!(
            sdr_holders(&Hardware::scan(&usb_dir), &usb_dir, &env, &set, None).len(),
            1
        );
    }

    #[test]
    fn levels_and_lines() {
        let c = check("sdrs", Level::Fail, "gone");
        assert_eq!(c.to_string(), "FAIL  sdrs         gone");
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
