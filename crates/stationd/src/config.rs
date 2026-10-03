// SPDX-License-Identifier: GPL-3.0-only
//! `stationd.toml`: the owner's local configuration (station-protocol §7).
//!
//! The hub can never change this file. An installed station's
//! file holds only the owner's policy and the SDR profile:
//!
//! ```toml
//! allow_frontend_control = true            # default true
//! allowed_admin_ops = []                   # restart_stationd, restart_backend, update, reboot_host
//! max_channels = 16                        # default 16, at most 64
//! max_waterfall_subscriptions = 4          # default 4, at most 16
//! [measurements]
//! enabled = true                           # measure the noise floor when the hub asks
//! [sdr]
//! profile = "rtlsdr"                       # rtlsdr | rtlsdr-hf (development builds also: siggen)
//! # center_frequency_hz = 98000000         # optional; default from the profile
//! ```
//!
//! Development builds ([`crate::flavour`]) also accept test-only settings,
//! which packaged builds reject:
//!
//! ```toml
//! state_dir = "/tmp/stationd-state"        # default /var/lib/coaxhub/stationd
//! [endpoints]                              # enrollment against a dev control plane
//! enroll_url = "https://localhost:7501"
//! station_api_url = "https://localhost:7502"
//! root_ca = "dev-root.pem"             # the ONLY root trusted for enrollment
//! [credentials]                            # static mode: no enrollment, no state dir
//! gateway_url = "https://localhost:7443"
//! cert = "station.pem"                     # client certificate chain (PEM, leaf first)
//! key = "station.key"                      # private key (PEM)
//! gateway_ca = "ca.pem"                    # the ONLY roots trusted for the gateway
//! [backend]
//! kind = "ka9q"                            # or "fake"
//! [backend.ka9q]                           # required with kind = "ka9q", refused otherwise
//! status_group = "239.77.0.6:5006"         # radiod's status group, or its status name
//! data_group = "239.77.0.4:5004"           # optional; default: asked from radiod
//! [measurements]
//! ignore_host_load = true                  # measuring never pauses for the host's load
//! ```
//!
//! - `[sdr]` is required unless a `[backend]` section chooses the backend.
//!   Without `[backend]`, stationd drives the bundled radiod on the fixed
//!   groups of [`crate::profile`].
//! - Without `[credentials]`, stationd runs on the credentials `stationd
//!   enroll` wrote to the state directory.
//! - Relative paths are resolved against the directory of the config file.
//!   Unknown keys are errors, so a typo never silently falls back to a
//!   default.

use std::fmt;
use std::path::{Path, PathBuf};

use backend_ka9q::backend::group::{self, DEFAULT_DATA_PORT, DEFAULT_STATUS_PORT};
use serde::Deserialize;
use station_proto::{limits, v1};
use tonic::transport::Uri;

use crate::flavour::{self, Flavour};
use crate::profile::{self, SdrProfile};

/// Where `stationd` looks for its config by default (§7).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/coaxhub/stationd.toml";

/// Default `max_channels` (§7).
pub const DEFAULT_MAX_CHANNELS: u32 = 16;

/// Default `max_waterfall_subscriptions` (§7).
pub const DEFAULT_MAX_WATERFALL_SUBSCRIPTIONS: u32 = 4;

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
    /// A test-only setting in a packaged build.
    #[error(
        "`{field}` is a development setting; packaged builds of stationd do not accept it \
         (remove it from stationd.toml)"
    )]
    DevelopmentOnly {
        /// The TOML key or section.
        field: &'static str,
    },
}

/// Admin operation names accepted in `allowed_admin_ops` (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdminOpName {
    RestartStationd,
    RestartBackend,
    Update,
    RebootHost,
}

impl From<AdminOpName> for v1::AdminOpKind {
    fn from(name: AdminOpName) -> Self {
        match name {
            AdminOpName::RestartStationd => Self::RestartStationd,
            AdminOpName::RestartBackend => Self::RestartBackend,
            AdminOpName::Update => Self::Update,
            AdminOpName::RebootHost => Self::RebootHost,
        }
    }
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

/// How to reach radiod (station-protocol §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ka9qSettings {
    /// radiod's status and command group.
    pub status_group: std::net::SocketAddr,
    /// radiod's RTP group; `None` asks radiod.
    pub data_group: Option<std::net::SocketAddr>,
}

impl Ka9qSettings {
    /// The bundled radiod on the package's fixed groups.
    pub fn bundled() -> Self {
        Self {
            status_group: profile::radiod_status_group(),
            data_group: Some(profile::radiod_data_group()),
        }
    }
}

/// The configured backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendConfig {
    /// `kind = "fake"`.
    Fake,
    /// radiod: the bundled one, or `[backend.ka9q]` in development builds.
    Ka9q(Ka9qSettings),
}

impl BackendConfig {
    /// The backend kind.
    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Fake => BackendKind::Fake,
            Self::Ka9q(_) => BackendKind::Ka9q,
        }
    }
}

impl fmt::Display for BackendConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fake => f.write_str("fake"),
            Self::Ka9q(k) => {
                write!(f, "ka9q, status {}", k.status_group)?;
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
    ka9q: Option<RawKa9q>,
}

impl RawBackend {
    fn validate(self) -> Result<BackendConfig, ConfigError> {
        match (self.kind, self.ka9q) {
            (BackendKind::Fake, None) => Ok(BackendConfig::Fake),
            (BackendKind::Fake, Some(_)) => Err(ConfigError::Invalid {
                field: "backend.ka9q",
                message: "only valid with kind = \"ka9q\"".to_owned(),
            }),
            (BackendKind::Ka9q, None) => Err(ConfigError::Invalid {
                field: "backend.ka9q",
                message: "kind = \"ka9q\" needs a [backend.ka9q] section with status_group"
                    .to_owned(),
            }),
            (BackendKind::Ka9q, Some(raw)) => {
                let status_group = group::parse_group(&raw.status_group, DEFAULT_STATUS_PORT)
                    .map_err(|e| ConfigError::Invalid {
                        field: "backend.ka9q.status_group",
                        message: e.to_string(),
                    })?;
                let data_group = raw
                    .data_group
                    .map(|d| group::parse_group(&d, DEFAULT_DATA_PORT))
                    .transpose()
                    .map_err(|e| ConfigError::Invalid {
                        field: "backend.ka9q.data_group",
                        message: e.to_string(),
                    })?;
                Ok(BackendConfig::Ka9q(Ka9qSettings {
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
    #[serde(default = "default_true")]
    enabled: bool,
    /// Development builds: the pause rule ignores the host's load.
    #[serde(default)]
    ignore_host_load: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSdr {
    profile: String,
    center_frequency_hz: Option<u64>,
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

fn default_true() -> bool {
    true
}

fn default_max_channels() -> u32 {
    DEFAULT_MAX_CHANNELS
}

fn default_max_waterfall_subscriptions() -> u32 {
    DEFAULT_MAX_WATERFALL_SUBSCRIPTIONS
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_true")]
    allow_frontend_control: bool,
    #[serde(default)]
    allowed_admin_ops: Vec<AdminOpName>,
    #[serde(default = "default_max_channels")]
    max_channels: u32,
    #[serde(default = "default_max_waterfall_subscriptions")]
    max_waterfall_subscriptions: u32,
    measurements: Option<RawMeasurements>,
    sdr: Option<RawSdr>,
    state_dir: Option<PathBuf>,
    endpoints: Option<RawEndpoints>,
    credentials: Option<RawCredentials>,
    backend: Option<RawBackend>,
}

/// What the owner lets the hub do (§4, §7). The hub may ask for less, never
/// more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPolicy {
    /// Whether `SetFrontend` is allowed.
    pub allow_frontend_control: bool,
    /// Admin operations the hub may trigger, sorted and without duplicates.
    pub allowed_admin_ops: Vec<v1::AdminOpKind>,
    /// Hard cap on simultaneous audio channels.
    pub max_channels: u32,
    /// Hard cap on simultaneous waterfall subscriptions.
    pub max_waterfall_subscriptions: u32,
    /// `[measurements] enabled`: whether the station measures the noise floor
    /// when the hub asks. The owner's switch; the hub
    /// can't change it.
    pub measurements: bool,
}

impl Default for LocalPolicy {
    /// The §7 defaults.
    fn default() -> Self {
        Self {
            allow_frontend_control: true,
            allowed_admin_ops: Vec::new(),
            max_channels: DEFAULT_MAX_CHANNELS,
            max_waterfall_subscriptions: DEFAULT_MAX_WATERFALL_SUBSCRIPTIONS,
            measurements: true,
        }
    }
}

impl LocalPolicy {
    /// Whether the owner allows admin operation `kind`.
    pub fn allows_admin_op(&self, kind: v1::AdminOpKind) -> bool {
        self.allowed_admin_ops.contains(&kind)
    }
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

/// `[sdr]`: the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdrSettings {
    /// The profile.
    pub profile: &'static SdrProfile,
    /// `center_frequency_hz` as configured.
    pub center_frequency_hz: Option<u64>,
}

impl SdrSettings {
    /// The configured centre frequency, else the profile's default; `None`
    /// for a front end without one.
    pub fn center_hz(&self) -> Option<u64> {
        self.profile
            .center
            .map(|c| self.center_frequency_hz.unwrap_or(c.default_hz))
    }
}

/// A validated `stationd.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The build flavour the file was checked against.
    pub flavour: Flavour,
    /// The owner's local policy.
    pub policy: LocalPolicy,
    /// `[sdr]`, if present.
    pub sdr: Option<SdrSettings>,
    /// The backend to drive.
    pub backend: BackendConfig,
    /// Whether `[backend]` chose the backend (development) rather than the
    /// profile.
    pub backend_explicit: bool,
    /// The state directory (credentials, assignment).
    pub state_dir: PathBuf,
    /// Where the daemon writes `status.json`.
    pub runtime_dir: PathBuf,
    /// The control-plane endpoints; `None` in a development build without
    /// `[endpoints]`.
    pub endpoints: Option<Endpoints>,
    /// Where the tunnel credentials come from.
    pub credentials: CredentialsMode,
    /// `[measurements] ignore_host_load` (development builds): measuring's
    /// pause rule ignores the host's load average (the measurement design §4.1), for the
    /// end-to-end scripts on a busy machine.
    pub ignore_host_load: bool,
}

impl Config {
    /// Reads and validates the config file at `path` for this build.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::load_as(path, Flavour::current())
    }

    /// Reads and validates the config file at `path` for `flavour`.
    pub fn load_as(path: &Path, flavour: Flavour) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse_as(&text, base, flavour).map_err(|e| match e {
            ConfigError::Parse { message, .. } => ConfigError::Parse {
                path: path.to_owned(),
                message,
            },
            other => other,
        })
    }

    /// Parses and validates config `text` for this build; relative paths
    /// are resolved against `base_dir`, and named files must exist.
    pub fn parse(text: &str, base_dir: &Path) -> Result<Self, ConfigError> {
        Self::parse_as(text, base_dir, Flavour::current())
    }

    /// Parses and validates config `text` for `flavour`.
    pub fn parse_as(text: &str, base_dir: &Path, flavour: Flavour) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: PathBuf::new(),
            message: e.to_string(),
        })?;
        if flavour.packaged {
            for (present, field) in [
                (raw.state_dir.is_some(), "state_dir"),
                (raw.endpoints.is_some(), "endpoints"),
                (raw.credentials.is_some(), "credentials"),
                (raw.backend.is_some(), "backend"),
                (
                    raw.measurements
                        .as_ref()
                        .is_some_and(|m| m.ignore_host_load),
                    "measurements.ignore_host_load",
                ),
            ] {
                if present {
                    return Err(ConfigError::DevelopmentOnly { field });
                }
            }
        }
        let mut policy = policy(
            raw.allow_frontend_control,
            raw.allowed_admin_ops,
            raw.max_channels,
            raw.max_waterfall_subscriptions,
        )?;
        policy.measurements = raw.measurements.as_ref().is_none_or(|m| m.enabled);
        let ignore_host_load = raw
            .measurements
            .as_ref()
            .is_some_and(|m| m.ignore_host_load);
        let sdr = raw.sdr.as_ref().map(|s| sdr(s, flavour)).transpose()?;
        let (backend, backend_explicit) = match (raw.backend, sdr) {
            (Some(b), _) => (b.validate()?, true),
            (None, Some(_)) => (BackendConfig::Ka9q(Ka9qSettings::bundled()), false),
            (None, None) => {
                return Err(ConfigError::Invalid {
                    field: "sdr",
                    message: "an [sdr] section with the receiver's `profile` is required"
                        .to_owned(),
                });
            }
        };
        let state_dir = match raw.state_dir {
            Some(p) => resolve("state_dir", base_dir, &p)?,
            None => PathBuf::from(flavour::DEFAULT_STATE_DIR),
        };
        let runtime_dir = if flavour.packaged {
            PathBuf::from(flavour::PACKAGED_RUNTIME_DIR)
        } else {
            state_dir.clone()
        };
        let endpoints = match (raw.endpoints, flavour.built_in) {
            (Some(e), _) => Some(Endpoints {
                enroll: Endpoint::parse("endpoints.enroll_url", &e.enroll_url)?,
                station_api: Endpoint::parse("endpoints.station_api_url", &e.station_api_url)?,
                root: TrustAnchor::File(existing_file("endpoints.root_ca", base_dir, &e.root_ca)?),
            }),
            (None, Some(b)) => Some(Endpoints {
                enroll: Endpoint::parse("built-in domain", &b.enroll_url())?,
                station_api: Endpoint::parse("built-in domain", &b.station_api_url())?,
                root: TrustAnchor::BuiltIn(b.root_ca_pem),
            }),
            (None, None) => None,
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
            flavour,
            policy,
            sdr,
            backend,
            backend_explicit,
            state_dir,
            runtime_dir,
            endpoints,
            credentials,
            ignore_host_load,
        })
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

fn policy(
    allow_frontend_control: bool,
    ops: Vec<AdminOpName>,
    max_channels: u32,
    max_waterfall_subscriptions: u32,
) -> Result<LocalPolicy, ConfigError> {
    if max_channels > limits::MAX_CHANNELS {
        return Err(ConfigError::Invalid {
            field: "max_channels",
            message: format!(
                "{max_channels} exceeds the protocol cap of {}",
                limits::MAX_CHANNELS
            ),
        });
    }
    if max_waterfall_subscriptions > limits::MAX_WATERFALL_SUBSCRIPTIONS {
        return Err(ConfigError::Invalid {
            field: "max_waterfall_subscriptions",
            message: format!(
                "{max_waterfall_subscriptions} exceeds the protocol cap of {}",
                limits::MAX_WATERFALL_SUBSCRIPTIONS
            ),
        });
    }
    let mut allowed_admin_ops: Vec<v1::AdminOpKind> =
        ops.into_iter().map(v1::AdminOpKind::from).collect();
    allowed_admin_ops.sort_unstable();
    allowed_admin_ops.dedup();
    Ok(LocalPolicy {
        allow_frontend_control,
        allowed_admin_ops,
        max_channels,
        max_waterfall_subscriptions,
        measurements: true,
    })
}

fn sdr(raw: &RawSdr, flavour: Flavour) -> Result<SdrSettings, ConfigError> {
    let profile =
        profile::by_name(&raw.profile, flavour.packaged).map_err(|e| ConfigError::Invalid {
            field: "sdr.profile",
            message: e.to_string(),
        })?;
    if let Some(hz) = raw.center_frequency_hz {
        let Some(range) = profile.center else {
            return Err(ConfigError::Invalid {
                field: "sdr.center_frequency_hz",
                message: format!(
                    "profile {:?} has no settable centre frequency",
                    profile.name
                ),
            });
        };
        if !(range.min_hz..=range.max_hz).contains(&hz) {
            return Err(ConfigError::Invalid {
                field: "sdr.center_frequency_hz",
                message: format!(
                    "{hz} Hz is outside {}..={} Hz for profile {:?}",
                    range.min_hz, range.max_hz, profile.name
                ),
            });
        }
    }
    Ok(SdrSettings {
        profile,
        center_frequency_hz: raw.center_frequency_hz,
    })
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
    use crate::flavour::BuiltIn;

    const BUILT_IN: BuiltIn = BuiltIn {
        domain: "coaxhub.example",
        root_ca_pem: "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n",
    };
    const DEV: Flavour = Flavour::development();
    const PKG: Flavour = Flavour::packaged_with(BUILT_IN);

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

    /// The earlier flat layout, as tests and scripts now write it.
    const STATIC: &str = r#"
        [credentials]
        gateway_url = "https://localhost:7443"
        cert = "station.pem"
        key = "station.key"
        gateway_ca = "ca.pem"
        [backend]
        kind = "fake"
    "#;

    /// An installed station.
    const OWNER: &str = r#"
        [sdr]
        profile = "rtlsdr"
    "#;

    fn parse(dir: &Dir, text: &str) -> Result<Config, ConfigError> {
        Config::parse_as(text, dir.path(), DEV)
    }

    fn with(extra: &str) -> String {
        format!("{extra}\n{STATIC}")
    }

    fn invalid_field(r: Result<Config, ConfigError>) -> &'static str {
        match r {
            Err(
                ConfigError::Invalid { field, .. }
                | ConfigError::File { field, .. }
                | ConfigError::DevelopmentOnly { field },
            ) => field,
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn static_config_gets_section_7_defaults() {
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
        assert_eq!(s.key, dir.path().join("station.key"));
        assert_eq!(c.key_path(), dir.path().join("station.key"));
        assert_eq!(s.gateway_ca, dir.path().join("ca.pem"));
        assert_eq!(c.policy, LocalPolicy::default());
        assert_eq!(c.backend, BackendConfig::Fake);
        assert!(c.backend_explicit);
        assert_eq!(c.sdr, None);
        assert_eq!(c.endpoints, None);
        assert_eq!(c.state_dir, PathBuf::from(flavour::DEFAULT_STATE_DIR));
        assert_eq!(c.runtime_dir, c.state_dir);
        assert_eq!(c.status_path(), c.state_dir.join("status.json"));
        assert_eq!(c.backend.kind(), BackendKind::Fake);
        assert_eq!(c.backend.to_string(), "fake");
        assert_eq!(BackendKind::Ka9q.to_string(), "ka9q");
    }

    #[test]
    fn owner_config_uses_the_bundled_radiod() {
        for flavour in [DEV, PKG] {
            let dir = Dir::new();
            let c = Config::parse_as(OWNER, dir.path(), flavour).expect("valid");
            assert_eq!(c.flavour, flavour);
            assert_eq!(c.backend, BackendConfig::Ka9q(Ka9qSettings::bundled()));
            assert!(!c.backend_explicit);
            assert_eq!(
                c.backend.to_string(),
                "ka9q, status 239.77.0.6:5006, data 239.77.0.4:5004"
            );
            let sdr = c.sdr.expect("sdr");
            assert_eq!(sdr.profile.name, "rtlsdr");
            assert_eq!(sdr.center_hz(), Some(98_000_000));
            assert_eq!(c.credentials, CredentialsMode::Enrolled);
            assert_eq!(
                c.key_path(),
                PathBuf::from("/var/lib/coaxhub/stationd/key.pem")
            );
        }
        // Packaged: built-in endpoints and runtime dir.
        let dir = Dir::new();
        let c = Config::parse_as(OWNER, dir.path(), PKG).expect("valid");
        let e = c.endpoints.clone().expect("built-in endpoints");
        assert_eq!(e.enroll.url, "https://enroll.coaxhub.example");
        assert_eq!(e.enroll.port, 443);
        assert_eq!(e.station_api.host, "station-api.coaxhub.example");
        assert_eq!(e.root, TrustAnchor::BuiltIn(BUILT_IN.root_ca_pem));
        assert_eq!(e.root.to_string(), "the built-in CoaxHub root");
        assert_eq!(
            c.status_path(),
            PathBuf::from("/run/coaxhub/stationd/status.json")
        );
        // Development: no endpoints unless configured.
        let c = Config::parse_as(OWNER, dir.path(), DEV).expect("valid");
        assert_eq!(c.endpoints, None);
    }

    #[test]
    fn packaged_builds_reject_test_only_settings() {
        let dir = Dir::new();
        let endpoints = "[endpoints]\nenroll_url = \"https://localhost:7501\"\n\
                         station_api_url = \"https://localhost:7502\"\nroot_ca = \"root.pem\"\n";
        let cases = [
            (format!("state_dir = \"/tmp/s\"\n{OWNER}"), "state_dir"),
            (format!("{OWNER}\n{endpoints}"), "endpoints"),
            (
                STATIC.replace("[backend]\n        kind = \"fake\"", OWNER),
                "credentials",
            ),
            (STATIC.to_owned(), "credentials"),
            (format!("{OWNER}\n[backend]\nkind = \"fake\"\n"), "backend"),
            (
                format!("{OWNER}\n[measurements]\nignore_host_load = true\n"),
                "measurements.ignore_host_load",
            ),
        ];
        for (text, field) in cases {
            // Accepted by development builds...
            parse(&dir, &text).unwrap_or_else(|e| panic!("dev must accept {field}: {e}"));
            // ...rejected by packaged ones, with a clear message.
            let e = Config::parse_as(&text, dir.path(), PKG);
            let msg = format!("{e:?}");
            assert_eq!(invalid_field(e), field, "{msg}");
        }
        let e = Config::parse_as("[sdr]\nprofile = \"siggen\"\n", dir.path(), PKG)
            .expect_err("siggen is dev only");
        assert_eq!(
            e.to_string(),
            "invalid config value `sdr.profile`: unknown SDR profile \"siggen\"; supported: rtlsdr, rtlsdr-hf"
        );
        assert!(
            ConfigError::DevelopmentOnly { field: "backend" }
                .to_string()
                .contains("packaged builds")
        );
        // An empty file needs [sdr] in both flavours.
        for flavour in [DEV, PKG] {
            assert_eq!(
                invalid_field(Config::parse_as("", dir.path(), flavour)),
                "sdr"
            );
        }
    }

    #[test]
    fn development_settings() {
        let dir = Dir::new();
        let text = format!(
            "state_dir = \"state\"\n{OWNER}\n[endpoints]\nenroll_url = \"https://localhost:7501\"\n\
             station_api_url = \"https://127.0.0.1:7502\"\nroot_ca = \"root.pem\"\n"
        );
        let c = parse(&dir, &text).expect("valid");
        assert_eq!(c.state_dir, dir.path().join("state"));
        assert_eq!(c.runtime_dir, dir.path().join("state"));
        let e = c.endpoints.expect("endpoints");
        assert_eq!(e.enroll.authority(), "localhost:7501");
        assert_eq!(e.station_api.host, "127.0.0.1");
        assert_eq!(e.root, TrustAnchor::File(dir.path().join("root.pem")));
        assert!(e.root.to_string().ends_with("root.pem"));
        // Errors name the field.
        assert_eq!(
            invalid_field(parse(&dir, &text.replace("root.pem", "missing.pem"))),
            "endpoints.root_ca"
        );
        assert_eq!(
            invalid_field(parse(
                &dir,
                &text.replace("https://localhost:7501", "http://x")
            )),
            "endpoints.enroll_url"
        );
        assert_eq!(
            invalid_field(parse(&dir, &text.replace("https://127.0.0.1:7502", "x"))),
            "endpoints.station_api_url"
        );
        assert_eq!(
            invalid_field(parse(&dir, &text.replace("\"state\"", "\"\""))),
            "state_dir"
        );
        assert!(matches!(
            parse(&dir, &format!("{text}\n[endpoints.extra]\n")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn sdr_section() {
        let dir = Dir::new();
        let sdr = |body: &str| format!("[sdr]\n{body}\n");
        let c = parse(
            &dir,
            &sdr("profile = \"rtlsdr-hf\"\ncenter_frequency_hz = 14200000"),
        )
        .expect("valid");
        assert_eq!(c.sdr.expect("sdr").center_hz(), Some(14_200_000));
        let c = parse(&dir, &sdr("profile = \"siggen\"")).expect("valid");
        assert_eq!(c.sdr.expect("sdr").center_hz(), None);
        for (body, field) in [
            (
                "profile = \"siggen\"\ncenter_frequency_hz = 10000000",
                "sdr.center_frequency_hz",
            ),
            (
                "profile = \"rtlsdr\"\ncenter_frequency_hz = 7000000",
                "sdr.center_frequency_hz",
            ),
            (
                "profile = \"rtlsdr-hf\"\ncenter_frequency_hz = 30000000",
                "sdr.center_frequency_hz",
            ),
            ("profile = \"airspy\"", "sdr.profile"),
        ] {
            assert_eq!(invalid_field(parse(&dir, &sdr(body))), field, "{body}");
        }
        assert!(matches!(
            parse(&dir, &sdr("profile = \"rtlsdr\"\ngain = 3")),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            parse(&dir, &sdr("center_frequency_hz = 1")),
            Err(ConfigError::Parse { .. })
        ));
        // [sdr] with an explicit backend: the backend wins.
        let c = parse(&dir, &format!("{STATIC}\n{}", sdr("profile = \"rtlsdr\""))).expect("ok");
        assert_eq!(c.backend, BackendConfig::Fake);
        assert!(c.sdr.is_some());
    }

    #[test]
    fn full_policy() {
        let dir = Dir::new();
        let text = with(
            r#"
            allow_frontend_control = false
            allowed_admin_ops = ["update", "restart_backend", "update"]
            max_channels = 64
            max_waterfall_subscriptions = 0
            "#,
        );
        let c = parse(&dir, &text).expect("valid");
        assert_eq!(
            c.policy,
            LocalPolicy {
                allow_frontend_control: false,
                allowed_admin_ops: vec![v1::AdminOpKind::RestartBackend, v1::AdminOpKind::Update],
                max_channels: 64,
                max_waterfall_subscriptions: 0,
                measurements: true,
            }
        );
        assert!(c.policy.allows_admin_op(v1::AdminOpKind::Update));
        let off = parse(&dir, &with("[measurements]\nenabled = false\n")).expect("valid");
        assert!(!off.policy.measurements);
        let on = parse(&dir, &with("[measurements]\n")).expect("valid");
        assert!(on.policy.measurements);
        assert!(!on.ignore_host_load);
        let ignore =
            parse(&dir, &with("[measurements]\nignore_host_load = true\n")).expect("valid");
        assert!(ignore.policy.measurements && ignore.ignore_host_load);
        // Off is not a development setting: packaged builds accept it.
        Config::parse_as(
            &format!("{OWNER}\n[measurements]\nignore_host_load = false\n"),
            dir.path(),
            PKG,
        )
        .expect("packaged builds accept the default");
        assert!(matches!(
            parse(&dir, &with("[measurements]\nmax_carriers = 3\n")),
            Err(ConfigError::Parse { .. })
        ));
        assert!(!c.policy.allows_admin_op(v1::AdminOpKind::RebootHost));
    }

    #[test]
    fn every_admin_op_name_maps() {
        let dir = Dir::new();
        let c = parse(
            &dir,
            &with(r#"allowed_admin_ops = ["reboot_host", "restart_stationd", "restart_backend", "update"]"#),
        )
        .expect("valid");
        assert_eq!(c.policy.allowed_admin_ops.len(), 4);
        assert!(matches!(
            parse(&dir, &with(r#"allowed_admin_ops = ["run_command"]"#)),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn caps_above_the_protocol_limits_are_rejected() {
        let dir = Dir::new();
        assert_eq!(
            invalid_field(parse(&dir, &with("max_channels = 65"))),
            "max_channels"
        );
        assert_eq!(
            invalid_field(parse(&dir, &with("max_waterfall_subscriptions = 17"))),
            "max_waterfall_subscriptions"
        );
        assert!(matches!(
            parse(&dir, &with("max_channels = -1")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn unknown_keys_and_backends_are_errors() {
        let dir = Dir::new();
        let e = parse(&dir, &with("max_chanels = 3")).expect_err("typo");
        assert!(e.to_string().contains("max_chanels"), "{e}");
        let e = parse(&dir, &STATIC.replace("\"fake\"", "\"sdrplay\"")).expect_err("kind");
        assert!(e.to_string().contains("sdrplay"), "{e}");
        // The earlier top-level credentials are unknown keys now.
        let e = parse(
            &dir,
            "gateway_url = \"https://localhost:7443\"\n[sdr]\nprofile = \"rtlsdr\"",
        )
        .expect_err("old layout");
        assert!(e.to_string().contains("gateway_url"), "{e}");
        assert!(matches!(
            parse(&dir, "not toml ="),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            parse(&dir, &STATIC.replace("gateway_ca", "ca")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn ka9q_backend_section() {
        let dir = Dir::new();
        let ka9q = |section: &str| {
            STATIC.replace(
                "kind = \"fake\"",
                &format!("kind = \"ka9q\"\n[backend.ka9q]\n{section}"),
            )
        };
        let c = parse(
            &dir,
            &ka9q("status_group = \"239.77.0.6:5006\"\ndata_group = \"239.77.0.4\""),
        )
        .expect("valid");
        assert_eq!(c.backend, BackendConfig::Ka9q(Ka9qSettings::bundled()));
        assert_eq!(c.backend.kind(), BackendKind::Ka9q);
        // A radiod status name, no data group.
        let c = parse(&dir, &ka9q("status_group = \"coaxhub-status.local\"")).expect("valid");
        let BackendConfig::Ka9q(k) = c.backend else {
            panic!("ka9q expected");
        };
        assert!(k.status_group.ip().is_multicast());
        assert_eq!(k.status_group.port(), 5006);
        assert_eq!(k.data_group, None);
        assert!(c.backend.to_string().ends_with("data from radiod"));
        // Errors.
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
            invalid_field(parse(&dir, &STATIC.replace("\"fake\"", "\"ka9q\""))),
            "backend.ka9q"
        );
        assert_eq!(
            invalid_field(parse(
                &dir,
                &format!("{STATIC}\n[backend.ka9q]\nstatus_group = \"239.77.0.6\"\n")
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
        let c = Config::load_as(&path, DEV).expect("load");
        let CredentialsMode::Static(s) = c.credentials else {
            panic!("static expected");
        };
        assert_eq!(s.gateway_ca, dir.path().join("ca.pem"));
        let e = Config::load(&dir.path().join("nope.toml")).expect_err("missing");
        assert!(matches!(e, ConfigError::Read { .. }), "{e}");
        std::fs::write(&path, "max_channels = \"x\"").expect("write");
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
            /// Arbitrary text never panics the parser, in either flavour.
            #[test]
            fn parse_never_panics(text in "\\PC{0,400}") {
                let dir = std::env::temp_dir();
                let _ = Config::parse_as(&text, &dir, DEV);
                let _ = Config::parse_as(&text, &dir, PKG);
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
