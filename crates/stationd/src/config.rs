// SPDX-License-Identifier: GPL-3.0-only
//! `stationd.toml`: the owner's local configuration (station-protocol §7).
//!
//! The hub can never change this file. It holds the gateway to
//! dial, the mTLS credentials, and the hard local caps the hub may ask less
//! than, never more:
//!
//! ```toml
//! gateway_url = "https://localhost:7443"   # static mode; otherwise StationControl.GetAssignment
//! cert = "/path/station.pem"               # client certificate chain (PEM, leaf first)
//! key = "/path/station.key"                # private key (PEM)
//! gateway_ca = "/path/ca.pem"              # the ONLY roots trusted for the gateway
//! allow_frontend_control = true            # default true
//! allowed_admin_ops = []                   # restart_stationd, restart_backend, update, reboot_host
//! max_channels = 16                        # default 16, at most 64
//! max_waterfall_subscriptions = 4          # default 4, at most 16
//! [backend]
//! kind = "ka9q"                            # or "fake"
//! [backend.ka9q]                           # required with kind = "ka9q", refused otherwise
//! status_group = "239.77.0.6:5006"         # radiod's status group, or its status name
//! data_group = "239.77.0.4:5004"           # optional; default: asked from radiod
//! ```
//!
//! `status_group` and `data_group` take an IP address with an optional port
//! (5006 and 5004 by default) or a radiod name such as
//! `coaxhub-status.local`, which is hashed into a `239.x.y.z` group exactly
//! as radiod does with its default `dns = no`
//! (`backend_ka9q::backend::group`).
//!
//! Relative paths are resolved against the directory of the config file.
//! Unknown keys are errors, so a typo never silently falls back to a
//! default.

use std::fmt;
use std::path::{Path, PathBuf};

use backend_ka9q::backend::group::{self, DEFAULT_DATA_PORT, DEFAULT_STATUS_PORT};
use serde::Deserialize;
use station_proto::{limits, v1};
use tonic::transport::Uri;

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

/// `[backend.ka9q]`: how to reach radiod (station-protocol §7, §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ka9qSettings {
    /// radiod's status and command group.
    pub status_group: std::net::SocketAddr,
    /// radiod's RTP group; `None` asks radiod.
    pub data_group: Option<std::net::SocketAddr>,
}

/// The configured backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackendConfig {
    /// `kind = "fake"`.
    Fake,
    /// `kind = "ka9q"` with its `[backend.ka9q]` settings.
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
    gateway_url: String,
    cert: PathBuf,
    key: PathBuf,
    gateway_ca: PathBuf,
    #[serde(default = "default_true")]
    allow_frontend_control: bool,
    #[serde(default)]
    allowed_admin_ops: Vec<AdminOpName>,
    #[serde(default = "default_max_channels")]
    max_channels: u32,
    #[serde(default = "default_max_waterfall_subscriptions")]
    max_waterfall_subscriptions: u32,
    backend: RawBackend,
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
}

impl Default for LocalPolicy {
    /// The §7 defaults.
    fn default() -> Self {
        Self {
            allow_frontend_control: true,
            allowed_admin_ops: Vec::new(),
            max_channels: DEFAULT_MAX_CHANNELS,
            max_waterfall_subscriptions: DEFAULT_MAX_WATERFALL_SUBSCRIPTIONS,
        }
    }
}

impl LocalPolicy {
    /// Whether the owner allows admin operation `kind`.
    pub fn allows_admin_op(&self, kind: v1::AdminOpKind) -> bool {
        self.allowed_admin_ops.contains(&kind)
    }
}

/// The hub gateway to dial, from `gateway_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    /// The URL as configured.
    pub url: String,
    /// Host name or IP address (IPv6 without brackets): the TLS server name.
    pub host: String,
    /// TCP port (443 when the URL has none).
    pub port: u16,
}

impl Gateway {
    /// Parses and checks `gateway_url`: `https`, a host, an optional port,
    /// no user info, path, query or fragment.
    pub fn parse(url: &str) -> Result<Self, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            field: "gateway_url",
            message,
        };
        let uri: Uri = url
            .parse()
            .map_err(|e| invalid(format!("{url:?} is not a valid URL: {e}")))?;
        if uri.scheme_str() != Some("https") {
            return Err(invalid(format!(
                "{url:?} must use https:// (the tunnel is always TLS)"
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

/// A validated `stationd.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The hub gateway.
    pub gateway: Gateway,
    /// Client certificate chain (PEM), resolved path.
    pub cert: PathBuf,
    /// Client private key (PEM), resolved path.
    pub key: PathBuf,
    /// Gateway trust bundle (PEM), resolved path.
    pub gateway_ca: PathBuf,
    /// The owner's local policy.
    pub policy: LocalPolicy,
    /// The backend to drive.
    pub backend: BackendConfig,
}

impl Config {
    /// Reads and validates the config file at `path`.
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

    /// Parses and validates config `text`; relative paths are resolved
    /// against `base_dir`, and the files must exist.
    pub fn parse(text: &str, base_dir: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: PathBuf::new(),
            message: e.to_string(),
        })?;
        let gateway = Gateway::parse(&raw.gateway_url)?;
        if raw.max_channels > limits::MAX_CHANNELS {
            return Err(ConfigError::Invalid {
                field: "max_channels",
                message: format!(
                    "{} exceeds the protocol cap of {}",
                    raw.max_channels,
                    limits::MAX_CHANNELS
                ),
            });
        }
        if raw.max_waterfall_subscriptions > limits::MAX_WATERFALL_SUBSCRIPTIONS {
            return Err(ConfigError::Invalid {
                field: "max_waterfall_subscriptions",
                message: format!(
                    "{} exceeds the protocol cap of {}",
                    raw.max_waterfall_subscriptions,
                    limits::MAX_WATERFALL_SUBSCRIPTIONS
                ),
            });
        }
        let mut allowed_admin_ops: Vec<v1::AdminOpKind> = raw
            .allowed_admin_ops
            .into_iter()
            .map(v1::AdminOpKind::from)
            .collect();
        allowed_admin_ops.sort_unstable();
        allowed_admin_ops.dedup();
        let backend = raw.backend.validate()?;
        Ok(Self {
            gateway,
            cert: existing_file("cert", base_dir, &raw.cert)?,
            key: existing_file("key", base_dir, &raw.key)?,
            gateway_ca: existing_file("gateway_ca", base_dir, &raw.gateway_ca)?,
            policy: LocalPolicy {
                allow_frontend_control: raw.allow_frontend_control,
                allowed_admin_ops,
                max_channels: raw.max_channels,
                max_waterfall_subscriptions: raw.max_waterfall_subscriptions,
            },
            backend,
        })
    }

    /// A warning if the private key file is readable by group or others
    /// (Unix; `None` elsewhere or when it cannot be checked).
    pub fn key_permission_warning(&self) -> Option<String> {
        key_permission_warning(&self.key)
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

fn existing_file(field: &'static str, base: &Path, path: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        });
    }
    let resolved = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
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
            for f in ["station.pem", "station.key", "ca.pem"] {
                std::fs::write(dir.path().join(f), "x").expect("write");
            }
            Self { dir }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    const MINIMAL: &str = r#"
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

    fn with(extra: &str) -> String {
        MINIMAL.replace("[backend]", &format!("{extra}\n[backend]"))
    }

    fn invalid_field(r: Result<Config, ConfigError>) -> &'static str {
        match r {
            Err(ConfigError::Invalid { field, .. } | ConfigError::File { field, .. }) => field,
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn minimal_config_gets_section_7_defaults() {
        let dir = Dir::new();
        let c = parse(&dir, MINIMAL).expect("valid");
        assert_eq!(
            c.gateway,
            Gateway {
                url: "https://localhost:7443".into(),
                host: "localhost".into(),
                port: 7443
            }
        );
        assert_eq!(c.cert, dir.path().join("station.pem"));
        assert_eq!(c.key, dir.path().join("station.key"));
        assert_eq!(c.gateway_ca, dir.path().join("ca.pem"));
        assert_eq!(c.policy, LocalPolicy::default());
        assert_eq!(c.backend, BackendConfig::Fake);
        assert_eq!(c.backend.kind(), BackendKind::Fake);
        assert_eq!(c.backend.to_string(), "fake");
        assert_eq!(BackendKind::Ka9q.to_string(), "ka9q");
    }

    #[test]
    fn full_config_from_the_brief() {
        let dir = Dir::new();
        let text = format!(
            r#"
            gateway_url = "https://gw.dev.localhost"
            cert = "{0}/station.pem"
            key = "{0}/station.key"
            gateway_ca = "{0}/ca.pem"
            allow_frontend_control = false
            allowed_admin_ops = ["update", "restart_backend", "update"]
            max_channels = 64
            max_waterfall_subscriptions = 0
            [backend]
            kind = "fake"
            "#,
            dir.path().display()
        );
        let c = parse(&dir, &text).expect("valid");
        assert_eq!(c.gateway.port, 443);
        assert_eq!(c.gateway.authority(), "gw.dev.localhost:443");
        assert_eq!(
            c.policy,
            LocalPolicy {
                allow_frontend_control: false,
                allowed_admin_ops: vec![v1::AdminOpKind::RestartBackend, v1::AdminOpKind::Update],
                max_channels: 64,
                max_waterfall_subscriptions: 0,
            }
        );
        assert!(c.policy.allows_admin_op(v1::AdminOpKind::Update));
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
        let e = parse(&dir, &MINIMAL.replace("\"fake\"", "\"sdrplay\"")).expect_err("kind");
        assert!(e.to_string().contains("sdrplay"), "{e}");
        let e = parse(
            &dir,
            &MINIMAL.replace("[backend]\n        kind = \"fake\"", ""),
        )
        .expect_err("backend section required");
        assert!(e.to_string().contains("backend"), "{e}");
        assert!(matches!(
            parse(&dir, "not toml ="),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn ka9q_backend_section() {
        let dir = Dir::new();
        let ka9q = |section: &str| {
            MINIMAL.replace(
                "kind = \"fake\"",
                &format!("kind = \"ka9q\"\n[backend.ka9q]\n{section}"),
            )
        };
        let c = parse(
            &dir,
            &ka9q("status_group = \"239.77.0.6:5006\"\ndata_group = \"239.77.0.4\""),
        )
        .expect("valid");
        assert_eq!(
            c.backend,
            BackendConfig::Ka9q(Ka9qSettings {
                status_group: "239.77.0.6:5006".parse().expect("addr"),
                data_group: Some("239.77.0.4:5004".parse().expect("addr")),
            })
        );
        assert_eq!(c.backend.kind(), BackendKind::Ka9q);
        assert_eq!(
            c.backend.to_string(),
            "ka9q, status 239.77.0.6:5006, data 239.77.0.4:5004"
        );
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
            invalid_field(parse(&dir, &MINIMAL.replace("\"fake\"", "\"ka9q\""))),
            "backend.ka9q"
        );
        assert_eq!(
            invalid_field(parse(
                &dir,
                &format!("{MINIMAL}\n[backend.ka9q]\nstatus_group = \"239.77.0.6\"\n")
            )),
            "backend.ka9q"
        );
        assert!(matches!(
            parse(&dir, &ka9q("status_group = \"239.77.0.6\"\nttl = 1")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn gateway_url_rules() {
        let ok = |u: &str| Gateway::parse(u).expect(u);
        assert_eq!(ok("https://127.0.0.1:7443").host, "127.0.0.1");
        let v6 = ok("https://[::1]:7443");
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.authority(), "[::1]:7443");
        assert_eq!(ok("https://gw.sa1.example/").port, 443);
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
                    Gateway::parse(bad),
                    Err(ConfigError::Invalid {
                        field: "gateway_url",
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
        let e = parse(&dir, &MINIMAL.replace("station.pem", "missing.pem"));
        assert_eq!(invalid_field(e), "cert");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let e = parse(&dir, &MINIMAL.replace("ca.pem", "sub"));
        assert!(
            matches!(e, Err(ConfigError::File { field: "gateway_ca", ref problem, .. }) if problem.contains("regular"))
        );
        let e = parse(&dir, &MINIMAL.replace("\"station.key\"", "\"\""));
        assert_eq!(invalid_field(e), "key");
    }

    #[test]
    fn load_reads_relative_to_the_config_file() {
        let dir = Dir::new();
        let path = dir.path().join("stationd.toml");
        std::fs::write(&path, MINIMAL).expect("write");
        let c = Config::load(&path).expect("load");
        assert_eq!(c.gateway_ca, dir.path().join("ca.pem"));
        let e = Config::load(&dir.path().join("nope.toml")).expect_err("missing");
        assert!(matches!(e, ConfigError::Read { .. }), "{e}");
        std::fs::write(&path, "gateway_url = 3").expect("write");
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
        let c = parse(&dir, MINIMAL).expect("valid");
        std::fs::set_permissions(&c.key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(c.key_permission_warning().expect("warning").contains("644"));
        std::fs::set_permissions(&c.key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(c.key_permission_warning(), None);
    }
}
