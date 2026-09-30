// SPDX-License-Identifier: GPL-3.0-only
//! Build flavours: development builds and packaged (release) builds
//! (station-protocol §7).
//!
//! | | development (default) | packaged (`--features packaged`) |
//! |---|---|---|
//! | `CoaxHub` domain and root | none built in | compiled in by `build.rs` |
//! | `state_dir`, `[endpoints]`, `[credentials]`, `[backend]` | accepted | rejected |
//! | SDR profiles | all, including `siggen` | production profiles only |
//! | runtime status file | `<state_dir>/status.json` | `/run/coaxhub/stationd/status.json` |
//! | radiod templates | `packaging/radiod-templates/` | `/usr/share/coaxhub/radiod-templates/` |

use std::path::PathBuf;

/// The state directory of an installed station (station-protocol §7).
pub const DEFAULT_STATE_DIR: &str = "/var/lib/coaxhub/stationd";

/// Where a packaged stationd writes its runtime status (systemd
/// `RuntimeDirectory=coaxhub/stationd`).
pub const PACKAGED_RUNTIME_DIR: &str = "/run/coaxhub/stationd";

/// Where the package installs the radiod templates.
pub const PACKAGED_TEMPLATES_DIR: &str = "/usr/share/coaxhub/radiod-templates";

/// Where `coaxhub-radiod.service` expects the rendered radiod config.
pub const DEFAULT_RADIOD_CONFIG: &str = "/run/coaxhub-radiod/radiod.conf";

/// The domain and root compiled into a packaged build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltIn {
    /// The `CoaxHub` domain (`COAXHUB_DOMAIN` at build time).
    pub domain: &'static str,
    /// The `CoaxHub` root certificate(s), PEM (`COAXHUB_ROOT_CA`).
    pub root_ca_pem: &'static str,
}

impl BuiltIn {
    /// `https://enroll.<domain>`.
    pub fn enroll_url(&self) -> String {
        format!("https://enroll.{}", self.domain)
    }

    /// `https://station-api.<domain>`.
    pub fn station_api_url(&self) -> String {
        format!("https://station-api.{}", self.domain)
    }
}

#[cfg(feature = "packaged")]
const BUILT_IN: Option<BuiltIn> = Some(BuiltIn {
    domain: env!("STATIOND_BUILT_IN_DOMAIN"),
    root_ca_pem: include_str!(concat!(env!("OUT_DIR"), "/coaxhub-root-ca.pem")),
});

#[cfg(not(feature = "packaged"))]
const BUILT_IN: Option<BuiltIn> = None;

/// Which rules a configuration is checked against. [`Flavour::current`] is
/// the flavour this binary was built as; tests also check the other one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flavour {
    /// Packaged: test-only settings are rejected.
    pub packaged: bool,
    /// The built-in domain and root (always `Some` when packaged).
    pub built_in: Option<BuiltIn>,
}

impl Flavour {
    /// This build.
    pub const fn current() -> Self {
        Self {
            packaged: cfg!(feature = "packaged"),
            built_in: BUILT_IN,
        }
    }

    /// A development build.
    pub const fn development() -> Self {
        Self {
            packaged: false,
            built_in: None,
        }
    }

    /// A packaged build with `built_in` (for tests of the packaged rules
    /// in a development build).
    pub const fn packaged_with(built_in: BuiltIn) -> Self {
        Self {
            packaged: true,
            built_in: Some(built_in),
        }
    }

    /// `"packaged"` or `"development"`.
    pub fn name(&self) -> &'static str {
        if self.packaged {
            "packaged"
        } else {
            "development"
        }
    }

    /// The default radiod template directory.
    pub fn templates_dir(&self) -> PathBuf {
        if self.packaged {
            PathBuf::from(PACKAGED_TEMPLATES_DIR)
        } else {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packaging/radiod-templates"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavours() {
        let b = BuiltIn {
            domain: "coaxhub.example",
            root_ca_pem: "",
        };
        assert_eq!(b.enroll_url(), "https://enroll.coaxhub.example");
        assert_eq!(b.station_api_url(), "https://station-api.coaxhub.example");
        let p = Flavour::packaged_with(b);
        assert_eq!(p.name(), "packaged");
        assert_eq!(p.templates_dir(), PathBuf::from(PACKAGED_TEMPLATES_DIR));
        let d = Flavour::development();
        assert_eq!(d.name(), "development");
        assert!(d.templates_dir().join("rtlsdr.conf").is_file());
        assert_eq!(
            Flavour::current().packaged,
            Flavour::current().built_in.is_some()
        );
    }
}
