// SPDX-License-Identifier: GPL-3.0-only
//! The `StationControl` client: `GetAssignment` and `RenewCertificate`
//! (station-protocol §1, §8).
//!
//! Every call dials `station-api` over TLS 1.3 (ALPN h2) with mutual TLS:
//! the station presents its current certificate, chain and key from the
//! state directory, and verifies the server against the state directory's
//! `gateway-ca.pem` **only** (never `WebPKI` or system roots). The control
//! plane identifies the station from the certificate alone; no request
//! carries an identity.
//!
//! Refusals (brief contract C): `PERMISSION_DENIED` `revoked` becomes
//! [`ControlError::Revoked`] (the daemon stops with exit code 3, as for
//! `Goodbye{REVOKED}`); `FAILED_PRECONDITION` becomes
//! [`ControlError::NotActive`].

use std::sync::Arc;
use std::time::Duration;

use station_proto::v1::station_control_client::StationControlClient;
use station_proto::v1::{GetAssignmentRequest, RenewCertificateRequest};
use tonic::transport::Channel;
use tracing::info;

use crate::config::{Config, CredentialsMode, Endpoint};
use crate::credentials::{CredentialSource as _, Credentials, CredentialsError, EnrolledSource};
use crate::enroll::{self, EnrollError};
use crate::rpc::{self, Failure};
use crate::state::{Assignment, NewCredentials, StateDir, StateError};
use crate::tls::Validity;

/// Timeout for `GetAssignment` (connect and call).
pub const ASSIGNMENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for `RenewCertificate` (connect and call).
pub const RENEW_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a `StationControl` call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ControlError {
    /// The control plane refused the certificate as revoked (or unknown,
    /// or the station was deleted).
    #[error("station-api refused this station's certificate (serial {serial}) as revoked")]
    Revoked {
        /// The serial of the certificate the call presented.
        serial: String,
    },
    /// The station is not `active` (disabled, killed or quarantined).
    #[error("station-api: this station is not active ({0})")]
    NotActive(String),
    /// The station has no usable credentials.
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    /// `station-api` could not be reached.
    #[error("cannot reach station-api at {endpoint}: {detail}")]
    Network {
        /// The endpoint.
        endpoint: String,
        /// What happened.
        detail: String,
    },
    /// The TLS handshake failed.
    #[error("TLS with station-api at {endpoint} failed: {detail}")]
    Tls {
        /// The endpoint.
        endpoint: String,
        /// What happened.
        detail: String,
    },
    /// No answer in time.
    #[error("station-api at {endpoint} did not answer in time")]
    Timeout {
        /// The endpoint.
        endpoint: String,
    },
    /// Another refusal.
    #[error("station-api refused the request ({code}): {message}")]
    Refused {
        /// The gRPC code.
        code: String,
        /// The server's message (escaped, truncated).
        message: String,
    },
    /// The answer is unusable.
    #[error("station-api sent an unusable response: {0}")]
    BadResponse(String),
    /// The new key could not be generated.
    #[error("cannot generate a new station key: {0}")]
    Key(String),
    /// The renewed credentials could not be stored.
    #[error("cannot store the renewed certificate: {0}")]
    Store(StateError),
    /// The assignment could not be stored.
    #[error("cannot store the assignment: {0}")]
    StoreAssignment(StateError),
}

impl ControlError {
    /// Whether the certificate was refused as revoked.
    pub fn is_revoked(&self) -> bool {
        matches!(self, Self::Revoked { .. })
    }
}

/// The result of [`StationApi::refresh_assignment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentUpdate {
    /// The home hub.
    pub assignment: Assignment,
    /// Whether `assignment.json` changed.
    pub changed: bool,
}

/// A successful renewal, already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renewed {
    /// The new certificate's serial, hex.
    pub serial: String,
    /// Its validity.
    pub validity: Validity,
    /// The serial of the certificate it replaced.
    pub previous_serial: String,
    /// Whether the gateway CA bundle rotated.
    pub ca_rotated: bool,
}

/// A `StationControl` client for an enrolled station.
#[derive(Debug, Clone)]
pub struct StationApi {
    endpoint: Endpoint,
    state: StateDir,
    /// Timeout for `GetAssignment`.
    pub assignment_timeout: Duration,
    /// Timeout for `RenewCertificate`.
    pub renew_timeout: Duration,
}

impl StationApi {
    /// A client for `endpoint` with the credentials in `state`.
    pub fn new(endpoint: Endpoint, state: StateDir) -> Self {
        Self {
            endpoint,
            state,
            assignment_timeout: ASSIGNMENT_TIMEOUT,
            renew_timeout: RENEW_TIMEOUT,
        }
    }

    /// The client `config` calls for: enrolled mode with a `station-api`
    /// endpoint (always in packaged builds; `[endpoints]` in development
    /// builds). `None` in static-credentials mode.
    pub fn from_config(config: &Config) -> Option<Self> {
        match (&config.credentials, &config.endpoints) {
            (CredentialsMode::Enrolled, Some(e)) => Some(Self::new(
                e.station_api.clone(),
                StateDir::new(&config.state_dir),
            )),
            _ => None,
        }
    }

    /// The endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn failure(&self, f: Failure, serial: &str) -> ControlError {
        let endpoint = self.endpoint.url.clone();
        match f {
            Failure::Network(detail) => ControlError::Network { endpoint, detail },
            Failure::Tls(detail) => ControlError::Tls { endpoint, detail },
            Failure::Timeout => ControlError::Timeout { endpoint },
            Failure::Status {
                code: tonic::Code::PermissionDenied,
                message,
            } if message.to_ascii_lowercase().contains("revoked") => ControlError::Revoked {
                serial: serial.to_owned(),
            },
            Failure::Status {
                code: tonic::Code::FailedPrecondition,
                message,
            } => ControlError::NotActive(message),
            Failure::Status { code, message } => ControlError::Refused {
                code: format!("{code:?}"),
                message,
            },
        }
    }

    async fn client(
        &self,
        credentials: &Credentials,
        timeout: Duration,
    ) -> Result<StationControlClient<Channel>, ControlError> {
        let channel = rpc::channel(
            &self.endpoint,
            Arc::clone(&credentials.tls.config),
            timeout,
            timeout,
        )
        .await
        .map_err(|f| self.failure(f, &credentials.tls.serial))?;
        Ok(StationControlClient::new(channel))
    }

    /// Calls `GetAssignment` with the current credentials.
    pub async fn get_assignment(&self) -> Result<Assignment, ControlError> {
        let credentials = EnrolledSource(self.state.clone()).load()?;
        let serial = credentials.tls.serial.clone();
        let timeout = self.assignment_timeout;
        let call = async {
            let mut client = self.client(&credentials, timeout).await?;
            let mut request = tonic::Request::new(GetAssignmentRequest {});
            request.set_timeout(timeout);
            rpc::with_timeout(timeout, client.get_assignment(request))
                .await
                .map_err(|f| self.failure(f, &serial))
        };
        let assignment =
            tokio::time::timeout(timeout, call)
                .await
                .map_err(|_| ControlError::Timeout {
                    endpoint: self.endpoint.url.clone(),
                })??;
        enroll::check_assignment(Some(assignment)).map_err(ControlError::BadResponse)
    }

    /// Calls `GetAssignment` and stores the answer in `assignment.json`
    /// when it changed.
    pub async fn refresh_assignment(&self) -> Result<AssignmentUpdate, ControlError> {
        let assignment = self.get_assignment().await?;
        let changed = self
            .state
            .write_assignment(&assignment)
            .map_err(ControlError::StoreAssignment)?;
        Ok(AssignmentUpdate {
            assignment,
            changed,
        })
    }

    /// Renews the certificate: a new P-256 key, a CSR, `RenewCertificate`
    /// with the current certificate, checks, then one atomic swap of key,
    /// certificate, chain (and a rotated CA bundle) in the state directory.
    pub async fn renew(&self) -> Result<Renewed, ControlError> {
        let credentials = EnrolledSource(self.state.clone()).load()?;
        let serial = credentials.tls.serial.clone();
        let key = enroll::generate_key().map_err(|e| match e {
            EnrollError::Key(m) => ControlError::Key(m),
            other => ControlError::Key(other.to_string()),
        })?;
        let timeout = self.renew_timeout;
        let call = async {
            let mut client = self.client(&credentials, timeout).await?;
            let mut request = tonic::Request::new(RenewCertificateRequest {
                csr_der: key.csr_der.clone(),
            });
            request.set_timeout(timeout);
            rpc::with_timeout(timeout, client.renew_certificate(request))
                .await
                .map_err(|f| self.failure(f, &serial))
        };
        let response =
            tokio::time::timeout(timeout, call)
                .await
                .map_err(|_| ControlError::Timeout {
                    endpoint: self.endpoint.url.clone(),
                })??;
        let cert = enroll::check_certificate(
            response.certificate,
            Some(credentials.tls.station_id),
            &key.public_key,
            crate::local_status::unix_now(),
        )
        .map_err(ControlError::BadResponse)?;
        let ca = response
            .gateway_ca_bundle_pem
            .as_deref()
            .map(enroll::check_ca_bundle)
            .transpose()
            .map_err(ControlError::BadResponse)?;
        self.state
            .swap_credentials(&NewCredentials {
                key_pem: &key.key_pem,
                cert_pem: &cert.cert_pem,
                chain_pem: &cert.chain_pem,
                gateway_ca_pem: ca.as_deref(),
            })
            .map_err(ControlError::Store)?;
        info!(
            serial = %cert.serial,
            previous_serial = %serial,
            not_after = cert.validity.not_after_unix_s,
            ca_rotated = ca.is_some(),
            "station certificate renewed and stored"
        );
        Ok(Renewed {
            serial: cert.serial,
            validity: cert.validity,
            previous_serial: serial,
            ca_rotated: ca.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    #[test]
    fn failures_map_to_errors() {
        let api = StationApi::new(
            Endpoint::parse("x", "https://station-api.example").expect("ep"),
            StateDir::new("/nonexistent"),
        );
        assert_eq!(api.endpoint().port, 443);
        let status = |code, m: &str| Failure::Status {
            code,
            message: m.to_owned(),
        };
        let e = api.failure(status(Code::PermissionDenied, "revoked"), "ab");
        assert!(
            matches!(&e, ControlError::Revoked { serial } if serial == "ab"),
            "{e}"
        );
        assert!(e.is_revoked());
        // PERMISSION_DENIED for another reason is not a revocation.
        let e = api.failure(status(Code::PermissionDenied, "nope"), "ab");
        assert!(matches!(e, ControlError::Refused { .. }), "{e}");
        let e = api.failure(
            status(Code::FailedPrecondition, "station is not active"),
            "ab",
        );
        assert!(matches!(e, ControlError::NotActive(_)), "{e}");
        assert!(!e.is_revoked());
        assert!(matches!(
            api.failure(Failure::Timeout, ""),
            ControlError::Timeout { .. }
        ));
        assert!(matches!(
            api.failure(Failure::Tls("x".into()), ""),
            ControlError::Tls { .. }
        ));
        let e = api.failure(Failure::Network("down".into()), "");
        assert!(e.to_string().contains("station-api.example"), "{e}");
    }
}
