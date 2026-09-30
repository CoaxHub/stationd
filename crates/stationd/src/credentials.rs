// SPDX-License-Identifier: GPL-3.0-only
//! Where the tunnel's credentials come from (station-protocol §8).
//!
//! The daemon asks its [`CredentialSource`] again before **every** connect
//! attempt, so a renewed certificate or a new assignment written to the
//! state directory is picked up by the next connection without a restart.
//!
//! - [`StaticSource`]: `[credentials]` in `stationd.toml` (development
//!   builds: tests, conformance, LAN setups).
//! - [`EnrolledSource`]: the state directory `stationd enroll` wrote (and
//!   certificate renewals update). A station is enrolled when every file is
//!   present and parseable and the key matches the certificate.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Config, ConfigError, CredentialsMode, Endpoint, StaticCredentials};
use crate::state::{self, StateDir, StateError};
use crate::tls::{self, ClientTls, CredentialFiles, TlsError};

/// What one connection attempt needs.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The gateway to dial.
    pub gateway: Endpoint,
    /// The home hub's id, when known (enrolled stations).
    pub hub_id: Option<String>,
    /// The mTLS client config, station ID and certificate validity.
    pub tls: ClientTls,
}

/// Why no credentials could be loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CredentialsError {
    /// Some state files are missing: the station is not enrolled.
    #[error(
        "this station is not enrolled (missing in {}: {}); run `stationd enroll`",
        dir.display(),
        missing.join(", ")
    )]
    NotEnrolled {
        /// The state directory.
        dir: PathBuf,
        /// The missing files.
        missing: Vec<&'static str>,
    },
    /// A state file cannot be read or parsed.
    #[error("enrollment state is unreadable ({0}); run `stationd enroll --force`")]
    State(#[from] StateError),
    /// The assignment names an unusable gateway.
    #[error("enrollment state is unusable ({0}); run `stationd enroll --force`")]
    Assignment(ConfigError),
    /// The certificate, key or CA bundle is unusable.
    #[error(transparent)]
    Tls(#[from] TlsError),
}

/// A source of tunnel credentials, re-read before every connect attempt.
pub trait CredentialSource: Send + Sync + fmt::Debug {
    /// Loads the current credentials.
    fn load(&self) -> Result<Credentials, CredentialsError>;

    /// A short description for logs and `status` (`"static"`,
    /// `"enrolled"`).
    fn kind(&self) -> &'static str;
}

/// `[credentials]` from `stationd.toml`.
#[derive(Debug, Clone)]
pub struct StaticSource(pub StaticCredentials);

impl CredentialSource for StaticSource {
    fn load(&self) -> Result<Credentials, CredentialsError> {
        let tls = tls::load_files(CredentialFiles {
            cert: &self.0.cert,
            chain: None,
            key: &self.0.key,
            ca: &self.0.gateway_ca,
        })?;
        Ok(Credentials {
            gateway: self.0.gateway.clone(),
            hub_id: None,
            tls,
        })
    }

    fn kind(&self) -> &'static str {
        "static"
    }
}

/// The enrolled state directory.
#[derive(Debug, Clone)]
pub struct EnrolledSource(pub StateDir);

impl CredentialSource for EnrolledSource {
    fn load(&self) -> Result<Credentials, CredentialsError> {
        let dir = &self.0;
        let present = dir.present();
        let missing: Vec<&'static str> = state::CREDENTIAL_FILES
            .into_iter()
            .filter(|f| !present.contains(f))
            .collect();
        if !missing.is_empty() {
            return Err(CredentialsError::NotEnrolled {
                dir: dir.path().to_owned(),
                missing,
            });
        }
        let assignment = dir.assignment()?;
        // Parsed for its validity only; the certificate is the identity.
        dir.station_id()?;
        let gateway = Endpoint::parse("assignment.json gateway_url", &assignment.gateway_url)
            .map_err(CredentialsError::Assignment)?;
        // A renewal swaps these as one set: read them under the swap lock,
        // from `next/` when a committed swap is not finished yet.
        let tls = {
            let _guard = state::swap_guard();
            let cert = dir.credential_file(state::CERT_FILE);
            let chain = dir.credential_file(state::CHAIN_FILE);
            let key = dir.credential_file(state::KEY_FILE);
            let ca = dir.credential_file(state::GATEWAY_CA_FILE);
            tls::load_files(CredentialFiles {
                cert: &cert,
                chain: Some(&chain),
                key: &key,
                ca: &ca,
            })?
        };
        Ok(Credentials {
            gateway,
            hub_id: Some(assignment.hub_id),
            tls,
        })
    }

    fn kind(&self) -> &'static str {
        "enrolled"
    }
}

/// The credential source `config` names.
pub fn source(config: &Config) -> Arc<dyn CredentialSource> {
    match &config.credentials {
        CredentialsMode::Static(s) => Arc::new(StaticSource(s.clone())),
        CredentialsMode::Enrolled => Arc::new(EnrolledSource(StateDir::new(&config.state_dir))),
    }
}

/// Loads the credentials `config` names.
pub fn load(config: &Config) -> Result<Credentials, CredentialsError> {
    source(config).load()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Assignment, Enrollment};
    use station_conformance::pki::TestPki;

    fn enroll(dir: &StateDir, pki: &TestPki, gateway_url: &str) {
        dir.write_enrollment(&Enrollment {
            station_id: pki.station_id,
            key_pem: &pki.station_key_pem,
            cert_pem: &pki.station_cert_pem,
            chain_pem: "",
            gateway_ca_pem: &pki.ca_pem,
            assignment: &Assignment {
                hub_id: "sa1".into(),
                gateway_url: gateway_url.into(),
            },
        })
        .expect("write");
    }

    #[test]
    fn enrolled_source() {
        let pki = TestPki::generate().expect("pki");
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path().join("state"));
        let src = EnrolledSource(dir.clone());
        assert_eq!(src.kind(), "enrolled");
        let e = src.load().expect_err("empty");
        assert!(
            matches!(&e, CredentialsError::NotEnrolled { missing, .. } if missing.len() == 6),
            "{e}"
        );
        assert!(e.to_string().contains("stationd enroll"), "{e}");
        enroll(&dir, &pki, "https://localhost:7443");
        let c = src.load().expect("enrolled");
        assert_eq!(c.tls.station_id, pki.station_id);
        assert_eq!(c.hub_id.as_deref(), Some("sa1"));
        assert_eq!(c.gateway.port, 7443);
        // A bad gateway URL in the assignment.
        enroll(&dir, &pki, "http://localhost");
        assert!(matches!(src.load(), Err(CredentialsError::Assignment(_))));
        // A key that does not match the certificate (a half-written
        // re-enrollment) is never used.
        enroll(&dir, &pki, "https://localhost:7443");
        let other = TestPki::generate().expect("pki");
        std::fs::write(dir.file(state::KEY_FILE), &other.station_key_pem).expect("write");
        assert!(matches!(src.load(), Err(CredentialsError::Tls(_))));
        // Corrupt station-id and assignment.
        enroll(&dir, &pki, "https://localhost:7443");
        std::fs::write(dir.file(state::STATION_ID_FILE), "x").expect("write");
        assert!(matches!(src.load(), Err(CredentialsError::State(_))));
        std::fs::remove_file(dir.file(state::CHAIN_FILE)).expect("rm");
        let e = src.load().expect_err("missing chain");
        assert!(e.to_string().contains("chain.pem"), "{e}");
    }

    #[test]
    fn enrolled_source_reads_a_staged_swap_consistently() {
        let pki = TestPki::generate().expect("pki");
        let renewed = TestPki::generate_for(pki.station_id).expect("pki");
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path().join("state"));
        enroll(&dir, &pki, "https://localhost:7443");
        let src = EnrolledSource(dir.clone());
        let before = src.load().expect("load").tls.serial;
        dir.stage_credentials(&state::NewCredentials {
            key_pem: &renewed.station_key_pem,
            cert_pem: &renewed.station_cert_pem,
            chain_pem: "",
            gateway_ca_pem: None,
        })
        .expect("stage");
        let staged = src.load().expect("staged set");
        assert_ne!(staged.tls.serial, before);
        // Half moved: the same, consistent new set.
        std::fs::rename(
            dir.path().join(state::NEXT_DIR).join(state::KEY_FILE),
            dir.file(state::KEY_FILE),
        )
        .expect("move key");
        assert_eq!(
            src.load().expect("half moved").tls.serial,
            staged.tls.serial
        );
        assert!(dir.complete_swap().expect("complete"));
        assert_eq!(src.load().expect("done").tls.serial, staged.tls.serial);
    }

    #[test]
    fn static_source() {
        let pki = TestPki::generate().expect("pki");
        let tmp = tempfile::tempdir().expect("tmp");
        let p = |n: &str, c: &str| {
            let path = tmp.path().join(n);
            std::fs::write(&path, c).expect("write");
            path
        };
        let s = StaticSource(StaticCredentials {
            gateway: Endpoint::parse("x", "https://localhost:1").expect("url"),
            cert: p("c.pem", &pki.station_cert_pem),
            key: p("k.pem", &pki.station_key_pem),
            gateway_ca: p("ca.pem", &pki.ca_pem),
        });
        assert_eq!(s.kind(), "static");
        let c = s.load().expect("load");
        assert_eq!(c.hub_id, None);
        assert_eq!(c.tls.station_id, pki.station_id);
    }
}
