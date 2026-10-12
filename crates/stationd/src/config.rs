// SPDX-License-Identifier: GPL-3.0-only
//! What `stationd` runs with (station-protocol §7).
//!
//! **A packaged station has no settings file** ([`Config::built_in`]): the
//! control-plane endpoints and the `CoaxHub` root are built in, the state
//! lives in `/var/lib/coaxhub/stationd`, the applied receivers in
//! `/var/lib/coaxhub/receivers/receivers.json`, and each receiver runs the
//! bundled radiod under systemd. Everything the owner sets lives in the
//! control plane and reaches the station as the hub's typed requests.
//!
//! **Development builds** read `stationd.toml` ([`Config::load`], `--config`,
//! default `/etc/coaxhub/stationd.toml`) for the settings tests and LAN
//! setups need; every key is optional:
//!
//! ```toml
//! state_dir = "/tmp/stationd-state"        # default /var/lib/coaxhub/stationd
//! [endpoints]                              # enrollment against a dev control plane
//! enroll_url = "https://localhost:7501"
//! station_api_url = "https://localhost:7502"
//! root_ca = "dev-root.pem"                 # the ONLY root trusted for enrollment
//! [credentials]                            # static mode: no enrollment
//! gateway_url = "https://localhost:7443"
//! cert = "station.pem"                     # client certificate chain (PEM, leaf first)
//! key = "station.key"                      # private key (PEM)
//! gateway_ca = "ca.pem"                    # the ONLY roots trusted for the gateway
//! [backend]
//! kind = "fake"                            # or "ka9q" (the default: the bundled radiod)
//! usb_devices = "usb"                      # where SDRs are read from (default
//!                                          # /sys/bus/usb/devices); simulated SDRs
//!                                          # are directories laid out like it
//! [backend.ka9q]                           # a radiod of your own, for every receiver;
//! status_group = "239.77.0.6:5006"         # stationd starts no unit. Its status group,
//! data_group = "239.77.0.4:5004"           # optional; default: asked from radiod
//! [measurements]
//! ignore_host_load = true                  # measuring never pauses for the host's load
//! ```
//!
//! - Without `[backend]`, or with `kind = "ka9q"` and no `[backend.ka9q]`,
//!   each receiver runs the bundled radiod on its slot's groups, started
//!   through systemd, as a packaged station does. With `kind = "fake"`, each
//!   receiver is a `FakeBackend` and nothing is started.
//! - With `state_dir`, the applied receivers live in
//!   `<state_dir>/receivers/receivers.json`.
//! - Without `[credentials]`, stationd runs on the credentials `stationd
//!   enroll` wrote to the state directory.
//! - Relative paths are resolved against the directory of the config file.
//!   Unknown keys are errors, so a typo never silently falls back to a
//!   default.

use std::fmt;
use std::path::{Path, PathBuf};

use backend_ka9q::backend::group::{self, DEFAULT_DATA_PORT, DEFAULT_STATUS_PORT};
use serde::Deserialize;
use tonic::transport::Uri;

use crate::flavour::{self, BuiltIn, Flavour};
use crate::hardware::USB_DEVICES_DIR;
use crate::receivers::store::RECEIVERS_FILE;

/// Where a development `stationd` looks for its config by default (§7).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/coaxhub/stationd.toml";

/// Why the configuration cannot be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config file {}: {source}", path.display())]
    Read {
        /// The config file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The file is not valid TOML or does not match the schema.
    #[error("invalid config file {}: {message}", path.display())]
    Parse {
        /// The config file.
        path: PathBuf,
        /// The TOML error, with line and column.
        message: String,
    },
    /// A value is out of bounds or malformed.
    #[error("invalid config value `{field}`: {message}")]
    Invalid {
        /// The TOML key.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// A configured file does not exist or is not a regular file.
    #[error("config value `{field}`: {}: {problem}", path.display())]
    File {
        /// The TOML key.
        field: &'static str,
        /// The resolved path.
        path: PathBuf,
        /// What is wrong.
        problem: String,
    },
}

/// The DSP backend `stationd` drives (`[backend] kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackendKind {
    /// `station_backend::FakeBackend`: synthetic media, no radio.
    Fake,
    /// `backend_ka9q::backend::Ka9qBackend`: ka9q-radio's radiod.
    Ka9q,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fake => f.write_str("fake"),
            Self::Ka9q => f.write_str("ka9q"),
        }
    }
}

/// A radiod of the developer's own (station-protocol §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ka9qSettings {
    /// radiod's status and command group.
    pub status_group: std::net::SocketAddr,
    /// radiod's RTP group; `None` asks radiod.
    pub data_group: Option<std::net::SocketAddr>,
}

/// How the receivers run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendConfig {
    /// The bundled radiod of each receiver, on its slot's groups, started
    /// through systemd (`coaxhub-radiod@<receiver_id>.service`).
    Bundled,
    /// `[backend.ka9q]` (development): every receiver on that radiod; no
    /// unit is started.
    External(Ka9qSettings),
    /// `kind = "fake"` (development): a `FakeBackend` per receiver.
    Fake,
}

impl BackendConfig {
    /// The backend kind.
    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Fake => BackendKind::Fake,
            Self::Bundled | Self::External(_) => BackendKind::Ka9q,
        }
    }
}

impl fmt::Display for BackendConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fake => f.write_str("fake"),
            Self::Bundled => f.write_str("ka9q, the bundled radiod of each receiver"),
            Self::External(k) => {
                write!(f, "ka9q, your radiod at status {}", k.status_group)?;
                match k.data_group {
                    Some(d) => write!(f, ", data {d}"),
                    None => f.write_str(", data from radiod"),
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKa9q {
    status_group: String,
    data_group: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackend {
    kind: BackendKind,
    usb_devices: Option<PathBuf>,
    ka9q: Option<RawKa9q>,
}

impl RawBackend {
    fn validate(&self) -> Result<BackendConfig, ConfigError> {
        match (self.kind, &self.ka9q) {
            (BackendKind::Fake, None) => Ok(BackendConfig::Fake),
            (BackendKind::Fake, Some(_)) => Err(ConfigError::Invalid {
                field: "backend.ka9q",
                message: "only valid with kind = \"ka9q\"".to_owned(),
            }),
            (BackendKind::Ka9q, None) => Ok(BackendConfig::Bundled),
            (BackendKind::Ka9q, Some(raw)) => {
                let status_group = group::parse_group(&raw.status_group, DEFAULT_STATUS_PORT)
                    .map_err(|e| ConfigError::Invalid {
                        field: "backend.ka9q.status_group",
                        message: e.to_string(),
                    })?;
                let data_group = raw
                    .data_group
                    .as_deref()
                    .map(|d| group::parse_group(d, DEFAULT_DATA_PORT))
                    .transpose()
                    .map_err(|e| ConfigError::Invalid {
                        field: "backend.ka9q.data_group",
                        message: e.to_string(),
                    })?;
                Ok(BackendConfig::External(Ka9qSettings {
                    status_group,
                    data_group,
                }))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeasurements {
    /// The pause rule ignores the host's load.
    #[serde(default)]
    ignore_host_load: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEndpoints {
    enroll_url: String,
    station_api_url: String,
    root_ca: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredentials {
    gateway_url: String,
    cert: PathBuf,
    key: PathBuf,
    gateway_ca: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    state_dir: Option<PathBuf>,
    endpoints: Option<RawEndpoints>,
    credentials: Option<RawCredentials>,
    backend: Option<RawBackend>,
    measurements: Option<RawMeasurements>,
}

/// An `https://host[:port]` service endpoint: the hub gateway, enrollment
/// or `StationControl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The URL as given.
    pub url: String,
    /// Host name or IP address (IPv6 without brackets): the TLS server name.
    pub host: String,
    /// TCP port (443 when the URL has none).
    pub port: u16,
}

impl Endpoint {
    /// Parses and checks `url` (the value of `field`): `https`, a host, an
    /// optional port, no user info, path, query or fragment.
    pub fn parse(field: &'static str, url: &str) -> Result<Self, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid { field, message };
        let uri: Uri = url
            .parse()
            .map_err(|e| invalid(format!("{url:?} is not a valid URL: {e}")))?;
        if uri.scheme_str() != Some("https") {
            return Err(invalid(format!(
                "{url:?} must use https:// (stationd only speaks TLS)"
            )));
        }
        let authority = uri
            .authority()
            .ok_or_else(|| invalid(format!("{url:?} has no host")))?;
        if authority.as_str().contains('@') {
            return Err(invalid(format!("{url:?} must not contain user info")));
        }
        if !matches!(uri.path(), "" | "/") || uri.query().is_some() || url.contains('#') {
            return Err(invalid(format!(
                "{url:?} must not have a path, query or fragment"
            )));
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        if host.is_empty() {
            return Err(invalid(format!("{url:?} has no host")));
        }
        let port = authority.port_u16().unwrap_or(443);
        if port == 0 {
            return Err(invalid(format!("{url:?} has port 0")));
        }
        Ok(Self {
            url: url.to_owned(),
            host,
            port,
        })
    }

    /// `host:port`, with brackets around an IPv6 address.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

/// The only root(s) enrollment trusts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustAnchor {
    /// Compiled into a packaged build.
    BuiltIn(&'static str),
    /// `[endpoints] root_ca` (development builds), resolved path.
    File(PathBuf),
}

impl fmt::Display for TrustAnchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltIn(_) => f.write_str("the built-in CoaxHub root"),
            Self::File(p) => write!(f, "{}", p.display()),
        }
    }
}

/// The control-plane endpoints (station-protocol §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// `Enrollment` (`https://enroll.<domain>`).
    pub enroll: Endpoint,
    /// `StationControl` (`https://station-api.<domain>`).
    pub station_api: Endpoint,
    /// The root enrollment is verified against.
    pub root: TrustAnchor,
}

impl Endpoints {
    fn built_in(b: BuiltIn) -> Result<Self, ConfigError> {
        Ok(Self {
            enroll: Endpoint::parse("built-in domain", &b.enroll_url())?,
            station_api: Endpoint::parse("built-in domain", &b.station_api_url())?,
            root: TrustAnchor::BuiltIn(b.root_ca_pem),
        })
    }
}

/// `[credentials]`: fixed gateway and certificate files, no enrollment
/// (development builds only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticCredentials {
    /// The hub gateway.
    pub gateway: Endpoint,
    /// Client certificate chain (PEM), resolved path.
    pub cert: PathBuf,
    /// Client private key (PEM), resolved path.
    pub key: PathBuf,
    /// Gateway trust bundle (PEM), resolved path.
    pub gateway_ca: PathBuf,
}

/// Where the tunnel credentials come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialsMode {
    /// `[credentials]` (development builds).
    Static(StaticCredentials),
    /// The state directory written by `stationd enroll`.
    Enrolled,
}

/// What `stationd` runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The build flavour.
    pub flavour: Flavour,
    /// How the receivers run.
    pub backend: BackendConfig,
    /// Where the SDRs are read from (`/sys/bus/usb/devices`).
    pub usb_devices: PathBuf,
    /// The state directory (credentials, assignment).
    pub state_dir: PathBuf,
    /// Where `receivers.json` lives.
    pub receivers_dir: PathBuf,
    /// Where the daemon writes `status.json`.
    pub runtime_dir: PathBuf,
    /// The control-plane endpoints; `None` in a development build without
    /// `[endpoints]`.
    pub endpoints: Option<Endpoints>,
    /// Where the tunnel credentials come from.
    pub credentials: CredentialsMode,
    /// `[measurements] ignore_host_load` (development builds): measuring's
    /// pause rule ignores the host's load average (§4.1), for the
    /// end-to-end scripts on a busy machine.
    pub ignore_host_load: bool,
}

impl Config {
    /// A packaged station's configuration: built-in values only (§7).
    pub fn built_in(built_in: BuiltIn) -> Result<Self, ConfigError> {
        Ok(Self {
            flavour: Flavour::packaged_with(built_in),
            backend: BackendConfig::Bundled,
            usb_devices: PathBuf::from(USB_DEVICES_DIR),
            state_dir: PathBuf::from(flavour::DEFAULT_STATE_DIR),
            receivers_dir: PathBuf::from(flavour::RECEIVERS_DIR),
            runtime_dir: PathBuf::from(flavour::PACKAGED_RUNTIME_DIR),
            endpoints: Some(Endpoints::built_in(built_in)?),
            credentials: CredentialsMode::Enrolled,
            ignore_host_load: false,
        })
    }

    /// Reads and validates a development config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&text, base).map_err(|e| match e {
            ConfigError::Parse { message, .. } => ConfigError::Parse {
                path: path.to_owned(),
                message,
            },
            other => other,
        })
    }

    /// Parses and validates development config `text`; relative paths are
    /// resolved against `base_dir`, and named files must exist.
    pub fn parse(text: &str, base_dir: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: PathBuf::new(),
            message: e.to_string(),
        })?;
        let backend = match &raw.backend {
            Some(b) => b.validate()?,
            None => BackendConfig::Bundled,
        };
        let usb_devices = match raw.backend.as_ref().and_then(|b| b.usb_devices.as_ref()) {
            Some(p) => resolve("backend.usb_devices", base_dir, p)?,
            None => PathBuf::from(USB_DEVICES_DIR),
        };
        let (state_dir, receivers_dir) = match &raw.state_dir {
            Some(p) => {
                let dir = resolve("state_dir", base_dir, p)?;
                let receivers = dir.join("receivers");
                (dir, receivers)
            }
            None => (
                PathBuf::from(flavour::DEFAULT_STATE_DIR),
                PathBuf::from(flavour::RECEIVERS_DIR),
            ),
        };
        let endpoints = match raw.endpoints {
            Some(e) => Some(Endpoints {
                enroll: Endpoint::parse("endpoints.enroll_url", &e.enroll_url)?,
                station_api: Endpoint::parse("endpoints.station_api_url", &e.station_api_url)?,
                root: TrustAnchor::File(existing_file("endpoints.root_ca", base_dir, &e.root_ca)?),
            }),
            None => None,
        };
        let credentials = match raw.credentials {
            Some(c) => CredentialsMode::Static(StaticCredentials {
                gateway: Endpoint::parse("credentials.gateway_url", &c.gateway_url)?,
                cert: existing_file("credentials.cert", base_dir, &c.cert)?,
                key: existing_file("credentials.key", base_dir, &c.key)?,
                gateway_ca: existing_file("credentials.gateway_ca", base_dir, &c.gateway_ca)?,
            }),
            None => CredentialsMode::Enrolled,
        };
        Ok(Self {
            flavour: Flavour::development(),
            backend,
            usb_devices,
            runtime_dir: state_dir.clone(),
            state_dir,
            receivers_dir,
            endpoints,
            credentials,
            ignore_host_load: raw.measurements.is_some_and(|m| m.ignore_host_load),
        })
    }

    /// `receivers.json`.
    pub fn receivers_path(&self) -> PathBuf {
        self.receivers_dir.join(RECEIVERS_FILE)
    }

    /// The private key file the tunnel uses.
    pub fn key_path(&self) -> PathBuf {
        match &self.credentials {
            CredentialsMode::Static(s) => s.key.clone(),
            CredentialsMode::Enrolled => self.state_dir.join(crate::state::KEY_FILE),
        }
    }

    /// A warning if the private key file is readable by group or others
    /// (Unix; `None` elsewhere or when it cannot be checked).
    pub fn key_permission_warning(&self) -> Option<String> {
        key_permission_warning(&self.key_path())
    }

    /// The runtime status file (`status.json`).
    pub fn status_path(&self) -> PathBuf {
        self.runtime_dir.join(crate::state::STATUS_FILE)
    }
}

#[cfg(unix)]
fn key_permission_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "private key {} has mode {:o}; it should be readable by its owner only (0600)",
            path.display(),
            mode & 0o777
        )
    })
}

#[cfg(not(unix))]
fn key_permission_warning(_path: &Path) -> Option<String> {
    None
}

fn resolve(field: &'static str, base: &Path, path: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        });
    }
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    })
}

fn existing_file(field: &'static str, base: &Path, path: &Path) -> Result<PathBuf, ConfigError> {
    let resolved = resolve(field, base, path)?;
    match std::fs::metadata(&resolved) {
        Ok(m) if m.is_file() => Ok(resolved),
        Ok(_) => Err(ConfigError::File {
            field,
            path: resolved,
            problem: "not a regular file".to_owned(),
        }),
        Err(e) => Err(ConfigError::File {
            field,
            path: resolved,
            problem: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir {
        dir: tempfile::TempDir,
    }

    impl Dir {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            for f in ["station.pem", "station.key", "ca.pem", "root.pem"] {
                std::fs::write(dir.path().join(f), "x").expect("write");
            }
            Self { dir }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    const STATIC: &str = r#"
        [credentials]
        gateway_url = "https://localhost:7443"
        cert = "station.pem"
        key = "station.key"
        gateway_ca = "ca.pem"
        [backend]
        kind = "fake"
    "#;

    fn parse(dir: &Dir, text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, dir.path())
    }

    fn invalid_field(r: Result<Config, ConfigError>) -> &'static str {
        match r {
            Err(ConfigError::Invalid { field, .. } | ConfigError::File { field, .. }) => field,
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn a_packaged_station_runs_on_built_in_values() {
        let b = BuiltIn {
            domain: "coaxhub.example",
            root_ca_pem: "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n",
        };
        let c = Config::built_in(b).expect("built in");
        assert!(c.flavour.packaged);
        assert_eq!(c.backend, BackendConfig::Bundled);
        assert_eq!(c.usb_devices, PathBuf::from("/sys/bus/usb/devices"));
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/coaxhub/stationd"));
        assert_eq!(
            c.receivers_path(),
            PathBuf::from("/var/lib/coaxhub/receivers/receivers.json")
        );
        assert_eq!(
            c.status_path(),
            PathBuf::from("/run/coaxhub/stationd/status.json")
        );
        assert_eq!(c.credentials, CredentialsMode::Enrolled);
        let e = c.endpoints.expect("endpoints");
        assert_eq!(e.enroll.url, "https://enroll.coaxhub.example");
        assert_eq!(e.enroll.port, 443);
        assert_eq!(e.station_api.host, "station-api.coaxhub.example");
        assert_eq!(e.root.to_string(), "the built-in CoaxHub root");
        assert!(!c.ignore_host_load);
    }

    #[test]
    fn an_empty_file_is_the_packaged_layout_in_development() {
        let dir = Dir::new();
        let c = parse(&dir, "").expect("empty");
        assert!(!c.flavour.packaged);
        assert_eq!(c.backend, BackendConfig::Bundled);
        assert_eq!(
            c.receivers_path(),
            PathBuf::from("/var/lib/coaxhub/receivers/receivers.json")
        );
        assert_eq!(c.runtime_dir, c.state_dir);
        assert_eq!(c.endpoints, None);
        assert_eq!(
            c.key_path(),
            PathBuf::from("/var/lib/coaxhub/stationd/key.pem")
        );
        assert_eq!(c.backend.kind(), BackendKind::Ka9q);
        assert!(c.backend.to_string().contains("bundled"));
    }

    #[test]
    fn static_credentials_and_the_fake_backend() {
        let dir = Dir::new();
        let c = parse(&dir, STATIC).expect("valid");
        let CredentialsMode::Static(s) = &c.credentials else {
            panic!("static expected");
        };
        assert_eq!(
            s.gateway,
            Endpoint {
                url: "https://localhost:7443".into(),
                host: "localhost".into(),
                port: 7443
            }
        );
        assert_eq!(s.cert, dir.path().join("station.pem"));
        assert_eq!(c.key_path(), dir.path().join("station.key"));
        assert_eq!(s.gateway_ca, dir.path().join("ca.pem"));
        assert_eq!(c.backend, BackendConfig::Fake);
        assert_eq!(c.backend.to_string(), "fake");
        assert_eq!(BackendKind::Ka9q.to_string(), "ka9q");
        assert_eq!(c.status_path(), c.state_dir.join("status.json"));
    }

    #[test]
    fn development_settings() {
        let dir = Dir::new();
        let text = "state_dir = \"state\"\n[endpoints]\nenroll_url = \"https://localhost:7501\"\n\
                    station_api_url = \"https://127.0.0.1:7502\"\nroot_ca = \"root.pem\"\n\
                    [backend]\nkind = \"fake\"\nusb_devices = \"usb\"\n\
                    [measurements]\nignore_host_load = true\n";
        let c = parse(&dir, text).expect("valid");
        assert_eq!(c.state_dir, dir.path().join("state"));
        assert_eq!(c.runtime_dir, dir.path().join("state"));
        assert_eq!(
            c.receivers_path(),
            dir.path().join("state/receivers/receivers.json")
        );
        assert_eq!(c.usb_devices, dir.path().join("usb"));
        assert!(c.ignore_host_load);
        let e = c.endpoints.expect("endpoints");
        assert_eq!(e.enroll.authority(), "localhost:7501");
        assert_eq!(e.station_api.host, "127.0.0.1");
        assert_eq!(e.root, TrustAnchor::File(dir.path().join("root.pem")));
        // Errors name the field.
        for (from, to, field) in [
            ("root.pem", "missing.pem", "endpoints.root_ca"),
            ("https://localhost:7501", "http://x", "endpoints.enroll_url"),
            ("https://127.0.0.1:7502", "x", "endpoints.station_api_url"),
            ("\"state\"", "\"\"", "state_dir"),
            ("\"usb\"", "\"\"", "backend.usb_devices"),
        ] {
            assert_eq!(invalid_field(parse(&dir, &text.replace(from, to))), field);
        }
        assert!(matches!(
            parse(&dir, &format!("{text}\n[endpoints.extra]\n")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn unknown_keys_are_errors() {
        let dir = Dir::new();
        // The capacity claims and measuring are not settings (§7).
        for text in [
            "max_chanels = 3",
            "max_channels = 16",
            "[measurements]\nenabled = true",
            "[receivers]\nprofile = \"rtlsdr\"",
            "not toml =",
        ] {
            assert!(
                matches!(parse(&dir, text), Err(ConfigError::Parse { .. })),
                "{text}"
            );
        }
        let e = parse(&dir, &STATIC.replace("\"fake\"", "\"sdrplay\"")).expect_err("kind");
        assert!(e.to_string().contains("sdrplay"), "{e}");
        assert!(matches!(
            parse(&dir, &STATIC.replace("gateway_ca", "ca")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn ka9q_backend_section() {
        let dir = Dir::new();
        let ka9q = |section: &str| format!("[backend]\nkind = \"ka9q\"\n[backend.ka9q]\n{section}");
        assert_eq!(
            parse(&dir, "[backend]\nkind = \"ka9q\"\n")
                .expect("bundled")
                .backend,
            BackendConfig::Bundled
        );
        let c = parse(
            &dir,
            &ka9q("status_group = \"239.77.0.6:5006\"\ndata_group = \"239.77.0.4\""),
        )
        .expect("valid");
        let BackendConfig::External(k) = c.backend else {
            panic!("external radiod expected");
        };
        assert_eq!(k.status_group.to_string(), "239.77.0.6:5006");
        assert_eq!(
            k.data_group.map(|d| d.to_string()).as_deref(),
            Some("239.77.0.4:5004")
        );
        assert_eq!(c.backend.kind(), BackendKind::Ka9q);
        assert!(c.backend.to_string().contains("your radiod"));
        // A radiod status name, no data group.
        let c = parse(&dir, &ka9q("status_group = \"coaxhub-status.local\"")).expect("valid");
        let BackendConfig::External(k) = c.backend else {
            panic!("external radiod expected");
        };
        assert!(k.status_group.ip().is_multicast());
        assert_eq!(k.data_group, None);
        assert!(c.backend.to_string().ends_with("data from radiod"));
        assert_eq!(
            invalid_field(parse(&dir, &ka9q("status_group = \"a b\""))),
            "backend.ka9q.status_group"
        );
        assert_eq!(
            invalid_field(parse(
                &dir,
                &ka9q("status_group = \"239.77.0.6\"\ndata_group = \"x:0\"")
            )),
            "backend.ka9q.data_group"
        );
        assert_eq!(
            invalid_field(parse(
                &dir,
                "[backend]\nkind = \"fake\"\n[backend.ka9q]\nstatus_group = \"239.77.0.6\"\n"
            )),
            "backend.ka9q"
        );
        assert!(matches!(
            parse(&dir, &ka9q("status_group = \"239.77.0.6\"\nttl = 1")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn endpoint_url_rules() {
        let ok = |u: &str| Endpoint::parse("x", u).expect(u);
        assert_eq!(ok("https://127.0.0.1:7443").host, "127.0.0.1");
        let v6 = ok("https://[::1]:7443");
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.authority(), "[::1]:7443");
        assert_eq!(ok("https://gw.sa1.example/").port, 443);
        assert_eq!(
            ok("https://gw.sa1.example").to_string(),
            "https://gw.sa1.example"
        );
        for bad in [
            "http://localhost:7443",
            "localhost:7443",
            "https://",
            "https://user@localhost",
            "https://localhost/path",
            "https://localhost?q=1",
            "https://localhost#frag",
            "https://localhost:0",
            "not a url",
        ] {
            assert!(
                matches!(
                    Endpoint::parse("credentials.gateway_url", bad),
                    Err(ConfigError::Invalid {
                        field: "credentials.gateway_url",
                        ..
                    })
                ),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn files_must_exist_and_be_regular() {
        let dir = Dir::new();
        let e = parse(&dir, &STATIC.replace("station.pem", "missing.pem"));
        assert_eq!(invalid_field(e), "credentials.cert");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let e = parse(&dir, &STATIC.replace("ca.pem", "sub"));
        assert!(
            matches!(e, Err(ConfigError::File { field: "credentials.gateway_ca", ref problem, .. }) if problem.contains("regular"))
        );
        let e = parse(&dir, &STATIC.replace("\"station.key\"", "\"\""));
        assert_eq!(invalid_field(e), "credentials.key");
    }

    #[test]
    fn load_reads_relative_to_the_config_file() {
        let dir = Dir::new();
        let path = dir.path().join("stationd.toml");
        std::fs::write(&path, STATIC).expect("write");
        let c = Config::load(&path).expect("load");
        let CredentialsMode::Static(s) = c.credentials else {
            panic!("static expected");
        };
        assert_eq!(s.gateway_ca, dir.path().join("ca.pem"));
        let e = Config::load(&dir.path().join("nope.toml")).expect_err("missing");
        assert!(matches!(e, ConfigError::Read { .. }), "{e}");
        std::fs::write(&path, "state_dir = 3").expect("write");
        let e = Config::load(&path).expect_err("bad");
        assert!(
            matches!(&e, ConfigError::Parse { path: p, .. } if *p == path),
            "{e}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn warns_about_readable_keys() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = Dir::new();
        let c = parse(&dir, STATIC).expect("valid");
        let key = c.key_path();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(c.key_permission_warning().expect("warning").contains("644"));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(c.key_permission_warning(), None);
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Arbitrary text never panics the parser.
            #[test]
            fn parse_never_panics(text in "\\PC{0,400}") {
                let dir = std::env::temp_dir();
                let _ = Config::parse(&text, &dir);
            }

            /// Arbitrary URLs never panic and accepted ones are https with a host.
            #[test]
            fn endpoint_parse_never_panics(url in "(https?://)?[a-z0-9.:@/?#\\[\\]-]{0,40}") {
                if let Ok(e) = Endpoint::parse("x", &url) {
                    prop_assert!(url.starts_with("https://"));
                    prop_assert!(!e.host.is_empty());
                    prop_assert!(e.port != 0);
                }
            }
        }
    }
}
