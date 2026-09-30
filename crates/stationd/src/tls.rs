// SPDX-License-Identifier: GPL-3.0-only
//! TLS to the hub gateway and the control plane (station-protocol §1).
//!
//! - TLS 1.3 only, `ring` crypto provider (rustls 0.23).
//! - For the gateway and `StationControl`, the client presents the station
//!   certificate chain and key (mTLS); enrollment presents none.
//! - Servers are authenticated against one explicit bundle **only**: the
//!   gateway CA bundle for the gateway and `StationControl`, the `CoaxHub`
//!   root for enrollment. No `WebPKI` roots, no system roots: `stationd`
//!   depends on neither `webpki-roots` nor `rustls-native-certs`.
//! - ALPN `h2` is offered and required; the server name (SNI and
//!   certificate check) is the host of the endpoint URL (a DNS name or an IP
//!   address).
//!
//! tonic's own TLS support cannot restrict the protocol version, so
//! [`Connector`] does TCP + TLS itself and hands tonic a finished TLS
//! stream (the channel then speaks HTTP/2 over it).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::{self, PemObject as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use station_proto::identity;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tonic::transport::Uri;
use uuid::Uuid;

use crate::config::Endpoint;

/// The only ALPN protocol offered and accepted.
pub const ALPN_H2: &[u8] = b"h2";

/// Why the TLS credentials cannot be loaded. Messages never contain key
/// material.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsError {
    /// The certificate chain file could not be read or parsed.
    #[error("cannot read certificate chain {}: {reason}", path.display())]
    Certificates {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// The private key file could not be read or parsed.
    #[error("cannot read private key {}: {reason}", path.display())]
    Key {
        /// The file.
        path: PathBuf,
        /// What went wrong (never the key itself).
        reason: String,
    },
    /// The gateway CA bundle could not be read, parsed or used.
    #[error("cannot use gateway CA bundle {}: {reason}", path.display())]
    GatewayCa {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// The leaf certificate does not carry exactly one station URI SAN.
    #[error("certificate {} does not identify a station: {source}", path.display())]
    StationId {
        /// The file.
        path: PathBuf,
        /// The identity error.
        source: identity::IdentityError,
    },
    /// rustls refused the credentials (e.g. the key does not match the
    /// certificate).
    #[error("cannot use the client certificate and key: {0}")]
    Credentials(rustls::Error),
    /// The endpoint host is not a valid TLS server name.
    #[error("host {0:?} is not a valid TLS server name")]
    ServerName(String),
}

/// Loaded client credentials and trust anchors.
#[derive(Debug, Clone)]
pub struct ClientTls {
    /// The rustls client configuration (TLS 1.3, ALPN h2, mTLS).
    pub config: Arc<ClientConfig>,
    /// The station ID from the certificate's URI SAN.
    pub station_id: Uuid,
    /// How many trust anchors the CA bundle contributed.
    pub trust_anchors: usize,
    /// The leaf certificate's validity.
    pub validity: Validity,
}

/// A certificate's validity period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    /// `notBefore`, Unix seconds.
    pub not_before_unix_s: i64,
    /// `notAfter`, Unix seconds.
    pub not_after_unix_s: i64,
}

impl Validity {
    /// The validity of a DER certificate.
    pub fn of_der(der: &[u8]) -> Option<Self> {
        use x509_parser::prelude::FromDer as _;
        let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).ok()?;
        Some(Self {
            not_before_unix_s: cert.validity().not_before.timestamp(),
            not_after_unix_s: cert.validity().not_after.timestamp(),
        })
    }
}

fn pem_reason(e: &pem::Error) -> String {
    match e {
        pem::Error::Io(io) => io.to_string(),
        pem::Error::NoItemsFound => "no PEM items of the expected type found".to_owned(),
        // Do not echo decoder details: they may quote file contents.
        _ => "malformed PEM".to_owned(),
    }
}

/// Reads every certificate in a PEM file (at least one).
fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, pem::Error> {
    let certs = CertificateDer::pem_file_iter(path)?.collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(pem::Error::NoItemsFound);
    }
    Ok(certs)
}

/// Parses every certificate in PEM text (at least one); errors never quote
/// the input.
pub fn parse_certs(pem_text: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs = CertificateDer::pem_slice_iter(pem_text.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| pem_reason(&e))?;
    if certs.is_empty() {
        return Err(pem_reason(&pem::Error::NoItemsFound));
    }
    Ok(certs)
}

/// A root store holding exactly `certs`: never `WebPKI` or system roots.
pub fn root_store(certs: Vec<CertificateDer<'static>>) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| format!("not a usable CA certificate: {e}"))?;
    }
    Ok(roots)
}

/// Where the files of a certificate, its key and the CA bundle are.
#[derive(Debug, Clone, Copy)]
pub struct CredentialFiles<'a> {
    /// The leaf certificate first, optionally followed by intermediates.
    pub cert: &'a Path,
    /// More intermediates (the state directory's `chain.pem`); may be empty.
    pub chain: Option<&'a Path>,
    /// The private key.
    pub key: &'a Path,
    /// The roots trusted for the server.
    pub ca: &'a Path,
}

/// Loads a certificate chain, its key and a CA bundle and builds the mTLS
/// client config.
pub fn load_files(files: CredentialFiles<'_>) -> Result<ClientTls, TlsError> {
    let mut chain = read_certs(files.cert).map_err(|e| TlsError::Certificates {
        path: files.cert.to_owned(),
        reason: pem_reason(&e),
    })?;
    if let Some(extra) = files.chain {
        match read_certs(extra) {
            Ok(more) => chain.extend(more),
            Err(pem::Error::NoItemsFound) => {}
            Err(e) => {
                return Err(TlsError::Certificates {
                    path: extra.to_owned(),
                    reason: pem_reason(&e),
                });
            }
        }
    }
    let leaf = chain.first().ok_or_else(|| TlsError::Certificates {
        path: files.cert.to_owned(),
        reason: "empty chain".to_owned(),
    })?;
    let station_id =
        identity::station_id_from_cert_der(leaf).map_err(|source| TlsError::StationId {
            path: files.cert.to_owned(),
            source,
        })?;
    let validity = Validity::of_der(leaf).ok_or_else(|| TlsError::Certificates {
        path: files.cert.to_owned(),
        reason: "the leaf certificate cannot be parsed".to_owned(),
    })?;
    let key = PrivateKeyDer::from_pem_file(files.key).map_err(|e| TlsError::Key {
        path: files.key.to_owned(),
        reason: pem_reason(&e),
    })?;
    let ca_error = |reason: String| TlsError::GatewayCa {
        path: files.ca.to_owned(),
        reason,
    };
    let roots = root_store(read_certs(files.ca).map_err(|e| ca_error(pem_reason(&e)))?)
        .map_err(ca_error)?;
    let trust_anchors = roots.len();
    let config = client_config(roots, Some((chain, key)))?;
    Ok(ClientTls {
        config: Arc::new(config),
        station_id,
        trust_anchors,
        validity,
    })
}

/// A TLS 1.3-only client config with the `ring` provider, trusting exactly
/// `roots`, offering ALPN h2, and presenting `client` (chain, key) when
/// given (mTLS).
pub fn client_config(
    roots: RootCertStore,
    client: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
) -> Result<ClientConfig, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(TlsError::Credentials)?
        .with_root_certificates(roots);
    let mut config = match client {
        Some((chain, key)) => builder
            .with_client_auth_cert(chain, key)
            .map_err(TlsError::Credentials)?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![ALPN_H2.to_vec()];
    Ok(config)
}

/// The TLS server name for a host (an IP address or a DNS name).
pub fn server_name(host: &str) -> Result<ServerName<'static>, TlsError> {
    ServerName::try_from(host.to_owned()).map_err(|_| TlsError::ServerName(host.to_owned()))
}

/// A failed connection attempt.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// TCP connect failed.
    #[error("TCP connect to {authority} failed: {source}")]
    Tcp {
        /// `host:port`.
        authority: String,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The TLS handshake failed (includes certificate verification).
    #[error("TLS handshake with {authority} failed: {source}")]
    Tls {
        /// `host:port`.
        authority: String,
        /// The I/O error wrapping the rustls error.
        source: std::io::Error,
    },
    /// The server did not select ALPN `h2`.
    #[error("{authority} did not negotiate ALPN h2")]
    Alpn {
        /// `host:port`.
        authority: String,
    },
}

/// Dials an endpoint: TCP, then TLS 1.3 (with mTLS when the config has a
/// client certificate). Used by tonic as the channel's connector (the URI
/// tonic passes in is ignored).
#[derive(Debug, Clone)]
pub struct Connector {
    config: Arc<ClientConfig>,
    host: String,
    port: u16,
    authority: String,
    server_name: ServerName<'static>,
}

impl Connector {
    /// A connector for `endpoint` using `config`.
    pub fn new(config: Arc<ClientConfig>, endpoint: &Endpoint) -> Result<Self, TlsError> {
        Ok(Self {
            config,
            host: endpoint.host.clone(),
            port: endpoint.port,
            authority: endpoint.authority(),
            server_name: server_name(&endpoint.host)?,
        })
    }

    /// Opens one TLS connection to the endpoint.
    pub async fn connect(&self) -> Result<TlsStream<TcpStream>, ConnectError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|source| ConnectError::Tcp {
                authority: self.authority.clone(),
                source,
            })?;
        // Latency matters more than packet count for request/response.
        let _ = tcp.set_nodelay(true);
        let tls = tokio_rustls::TlsConnector::from(Arc::clone(&self.config))
            .connect(self.server_name.clone(), tcp)
            .await
            .map_err(|source| ConnectError::Tls {
                authority: self.authority.clone(),
                source,
            })?;
        if tls.get_ref().1.alpn_protocol() != Some(ALPN_H2) {
            return Err(ConnectError::Alpn {
                authority: self.authority.clone(),
            });
        }
        Ok(tls)
    }
}

impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<TlsStream<TcpStream>>;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.connect().await.map(TokioIo::new) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_conformance::pki::TestPki;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).expect("write");
        p
    }

    struct Files {
        cert: PathBuf,
        key: PathBuf,
        ca: PathBuf,
    }

    fn config(dir: &Path, cert: &str, key: &str, ca: &str) -> Files {
        Files {
            cert: write(dir, "cert.pem", cert),
            key: write(dir, "key.pem", key),
            ca: write(dir, "ca.pem", ca),
        }
    }

    fn load(f: &Files) -> Result<ClientTls, TlsError> {
        load_files(CredentialFiles {
            cert: &f.cert,
            chain: None,
            key: &f.key,
            ca: &f.ca,
        })
    }

    #[test]
    fn loads_station_credentials_and_only_the_given_roots() {
        let pki = TestPki::generate().expect("pki");
        let dir = tempfile::tempdir().expect("dir");
        let c = config(
            dir.path(),
            &pki.station_cert_pem,
            &pki.station_key_pem,
            &pki.ca_pem,
        );
        let tls = load(&c).expect("load");
        assert_eq!(tls.station_id, pki.station_id);
        // Exactly the one CA from gateway_ca: no WebPKI or system roots.
        assert_eq!(tls.trust_anchors, 1);
        assert_eq!(tls.config.alpn_protocols, vec![b"h2".to_vec()]);
        // Two CAs in the bundle: both, and nothing else.
        let other = TestPki::generate().expect("pki");
        let c2 = config(
            dir.path(),
            &pki.station_cert_pem,
            &pki.station_key_pem,
            &format!("{}{}", pki.ca_pem, other.ca_pem),
        );
        assert_eq!(load(&c2).expect("load").trust_anchors, 2);
    }

    #[test]
    fn rejects_bad_credentials_without_echoing_keys() {
        let pki = TestPki::generate().expect("pki");
        let other = TestPki::generate().expect("pki");
        let dir = tempfile::tempdir().expect("dir");
        let d = dir.path();
        let (cert, key, ca) = (&pki.station_cert_pem, &pki.station_key_pem, &pki.ca_pem);

        let e = load(&config(d, "", key, ca)).expect_err("no cert");
        assert!(matches!(e, TlsError::Certificates { .. }), "{e}");
        // A certificate without the station URI SAN (the hub's).
        let e = load(&config(d, &pki.hub_cert_pem, &pki.hub_key_pem, ca)).expect_err("no SAN");
        assert!(matches!(e, TlsError::StationId { .. }), "{e}");
        let e = load(&config(d, cert, "garbage", ca)).expect_err("no key");
        assert!(matches!(e, TlsError::Key { .. }), "{e}");
        let broken_key = key.replace("PRIVATE KEY-----\n", "PRIVATE KEY-----\n!!!!");
        let e = load(&config(d, cert, &broken_key, ca)).expect_err("bad key");
        assert!(!e.to_string().contains("!!!!"), "{e}");
        // Key of another station.
        let e = load(&config(d, cert, &other.station_key_pem, ca)).expect_err("mismatch");
        assert!(matches!(e, TlsError::Credentials(_)), "{e}");
        // An empty CA bundle never falls back to any other roots.
        let e = load(&config(d, cert, key, "")).expect_err("no CA");
        assert!(matches!(e, TlsError::GatewayCa { .. }), "{e}");
        let bad_ca = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let e = load(&config(d, cert, key, bad_ca)).expect_err("bad CA");
        assert!(matches!(e, TlsError::GatewayCa { .. }), "{e}");
        for e in [
            load(&config(d, cert, "garbage", ca)).expect_err("x"),
            load(&config(d, cert, &broken_key, ca)).expect_err("x"),
        ] {
            assert!(!e.to_string().contains("BEGIN"), "{e}");
        }
    }

    #[test]
    fn server_names() {
        assert!(matches!(
            server_name("127.0.0.1"),
            Ok(ServerName::IpAddress(_))
        ));
        assert!(matches!(server_name("::1"), Ok(ServerName::IpAddress(_))));
        assert!(matches!(
            server_name("gw.sa1.example"),
            Ok(ServerName::DnsName(_))
        ));
        assert!(matches!(
            server_name("bad name"),
            Err(TlsError::ServerName(_))
        ));
    }

    fn files<'a>(f: &'a Files, chain: &'a Path) -> CredentialFiles<'a> {
        CredentialFiles {
            cert: &f.cert,
            chain: Some(chain),
            key: &f.key,
            ca: &f.ca,
        }
    }

    #[test]
    fn chain_file_validity_and_bundles() {
        let pki = TestPki::generate().expect("pki");
        let dir = tempfile::tempdir().expect("dir");
        let f = config(
            dir.path(),
            &pki.station_cert_pem,
            &pki.station_key_pem,
            &pki.ca_pem,
        );
        // An empty chain.pem is fine; a CA in it is sent as an intermediate.
        let chain = write(dir.path(), "chain.pem", "");
        let tls = load_files(files(&f, &chain)).expect("empty chain");
        assert!(tls.validity.not_after_unix_s > tls.validity.not_before_unix_s);
        write(dir.path(), "chain.pem", &pki.ca_pem);
        assert!(load_files(files(&f, &chain)).is_ok());
        write(
            dir.path(),
            "chain.pem",
            "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
        );
        let e = load_files(files(&f, &chain)).expect_err("bad chain");
        assert!(matches!(e, TlsError::Certificates { .. }), "{e}");
        let e = load_files(files(&f, &dir.path().join("missing.pem"))).expect_err("missing");
        assert!(matches!(e, TlsError::Certificates { .. }), "{e}");

        assert_eq!(parse_certs(&pki.ca_pem).expect("one").len(), 1);
        assert!(parse_certs("").is_err());
        assert!(
            parse_certs("-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n").is_err()
        );
        let bad = CertificateDer::from(vec![1_u8, 2, 3]);
        assert!(root_store(vec![bad]).is_err());
        assert_eq!(Validity::of_der(&[1, 2, 3]), None);
        // Without a client certificate (enrollment).
        let roots = root_store(parse_certs(&pki.ca_pem).expect("ca")).expect("roots");
        let c = client_config(roots, None).expect("config");
        assert!(!c.client_auth_cert_resolver.has_certs());
        assert_eq!(c.alpn_protocols, vec![b"h2".to_vec()]);
    }
}
