// SPDX-License-Identifier: GPL-3.0-only
//! A throwaway `CoaxHub`-like PKI (generated in memory with rcgen) and an
//! in-test `Enrollment` gRPC server over TLS 1.3 on loopback.
//!
//! Hierarchy: a root; a station intermediate that
//! signs station certificates from CSRs; a server intermediate that signs
//! the enrollment server's and the hub gateway's certificates.
#![allow(dead_code, clippy::missing_panics_doc)]
// Debug formatting quotes paths as TOML strings on purpose.
#![allow(clippy::unnecessary_debug_formatting)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
use station_conformance::{FakeHub, HubTls, TlsVersions};
use station_proto::identity;
use station_proto::v1::enrollment_server::{Enrollment, EnrollmentServer};
use station_proto::v1::{Assignment, EnrollRequest, EnrollResponse, StationCertificate};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::server::Connected;
use tonic::{Request, Response, Status};
use uuid::Uuid;

/// A CA: its certificate and an issuer.
pub struct Ca {
    pub pem: String,
    pub der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

fn ca(name: &str, parent: Option<&Ca>, pathlen: u8) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(pathlen));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let key = KeyPair::generate().expect("key");
    let cert = match parent {
        Some(p) => params.signed_by(&key, &p.issuer).expect("sign"),
        None => params.self_signed(&key).expect("self-sign"),
    };
    Ca {
        pem: cert.pem(),
        der: cert.der().to_vec(),
        issuer: Issuer::new(params, key),
    }
}

/// Root, station intermediate, server intermediate.
pub struct TestCa {
    pub root: Ca,
    pub station: Ca,
    pub server: Ca,
}

/// A server certificate chain (leaf + server intermediate) and its key.
pub struct ServerCert {
    pub chain_pem: String,
    pub key_pem: String,
}

impl Default for TestCa {
    fn default() -> Self {
        Self::new()
    }
}

impl TestCa {
    pub fn new() -> Self {
        let root = ca("Test CoaxHub Root CA", None, 1);
        let station = ca("Test station CA", Some(&root), 0);
        let server = ca("Test server CA", Some(&root), 0);
        Self {
            root,
            station,
            server,
        }
    }

    /// A server certificate for `dns` names and loopback IPs.
    pub fn server_cert(&self, dns: &[&str]) -> ServerCert {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.distinguished_name.push(DnType::CommonName, "server");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        for name in dns {
            params
                .subject_alt_names
                .push(SanType::DnsName(Ia5String::try_from(*name).expect("dns")));
        }
        if dns.contains(&"localhost") {
            params
                .subject_alt_names
                .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        }
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.server.issuer).expect("sign");
        ServerCert {
            chain_pem: format!("{}{}", cert.pem(), self.server.pem),
            key_pem: key.serialize_pem(),
        }
    }

    /// Signs a station CSR the way the CA does: the subject and requested
    /// extensions are ignored; SAN = the station URI, clientAuth, 30 days.
    pub fn sign_station(&self, csr_der: &[u8], id: Uuid) -> Vec<u8> {
        let csr =
            CertificateSigningRequestParams::from_der(&csr_der.to_vec().into()).expect("valid CSR");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params
            .distinguished_name
            .push(DnType::CommonName, "station");
        params.subject_alt_names = vec![SanType::URI(
            Ia5String::try_from(identity::station_uri(id).as_str()).expect("uri"),
        )];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let now = time_now();
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + time::Duration::days(30);
        let csr = CertificateSigningRequestParams {
            params,
            public_key: csr.public_key,
        };
        csr.signed_by(&self.station.issuer)
            .expect("sign")
            .der()
            .to_vec()
    }

    /// A fake hub gateway whose certificate comes from the server
    /// intermediate and that trusts the root for station certificates.
    pub async fn hub(&self) -> FakeHub {
        let cert = self.server_cert(&["localhost"]);
        FakeHub::start(&HubTls {
            cert_chain_pem: cert.chain_pem,
            key_pem: cert.key_pem,
            client_ca_pem: self.root.pem.clone(),
            versions: TlsVersions::Tls13Only,
        })
        .await
        .expect("hub")
    }

    /// Writes the root to `dir/root.pem`.
    pub fn write_root(&self, dir: &Path) -> PathBuf {
        let p = dir.join("root.pem");
        std::fs::write(&p, &self.root.pem).expect("write root");
        p
    }
}

fn time_now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

/// What the server does with the next requests.
#[derive(Clone)]
pub enum Behaviour {
    /// Sign the CSR and assign `gateway_url`.
    Sign { gateway_url: String },
    /// Fail with this status.
    Fail(Status),
    /// Sign, but for another key (a broken CA).
    WrongKey { gateway_url: String },
    /// Sign, but return another station id.
    WrongId { gateway_url: String },
}

/// One received request.
#[derive(Debug, Clone)]
pub struct Seen {
    pub token: String,
    pub daemon_version: String,
    pub csr_len: usize,
}

struct Service {
    ca: Arc<TestCa>,
    station_id: Uuid,
    behaviour: Arc<Mutex<Behaviour>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[tonic::async_trait]
impl Enrollment for Service {
    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let r = request.into_inner();
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen {
                token: r.token.clone(),
                daemon_version: r.daemon_version.clone(),
                csr_len: r.csr_der.len(),
            });
        let behaviour = self
            .behaviour
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (csr, id, gateway_url) = match behaviour {
            Behaviour::Fail(s) => return Err(s),
            Behaviour::Sign { gateway_url } => (r.csr_der, self.station_id, gateway_url),
            Behaviour::WrongKey { gateway_url } => {
                let other = stationd::enroll::generate_key().expect("key");
                (other.csr_der, self.station_id, gateway_url)
            }
            Behaviour::WrongId { gateway_url } => (r.csr_der, Uuid::new_v4(), gateway_url),
        };
        let der = self.ca.sign_station(&csr, id);
        Ok(Response::new(EnrollResponse {
            station_id: self.station_id.to_string(),
            certificate: Some(StationCertificate {
                certificate_der: der,
                chain_der: vec![self.ca.station.der.clone()],
                not_after_unix_s: 0,
            }),
            gateway_ca_bundle_pem: self.ca.root.pem.clone(),
            assignment: Some(Assignment {
                hub_id: "sa1".into(),
                gateway_url,
            }),
        }))
    }
}

/// A TLS stream tonic can serve.
struct Io(TlsStream<TcpStream>);

impl Connected for Io {
    type ConnectInfo = ();
    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for Io {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// The in-test enrollment server.
pub struct EnrollServer {
    pub addr: SocketAddr,
    pub station_id: Uuid,
    behaviour: Arc<Mutex<Behaviour>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    handshake_errors: Arc<Mutex<Vec<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for EnrollServer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn server_config(cert: &ServerCert) -> Arc<ServerConfig> {
    let chain = CertificateDer::pem_slice_iter(cert.chain_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .expect("chain");
    let key = PrivateKeyDer::from_pem_slice(cert.key_pem.as_bytes()).expect("key");
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("cert");
    config.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(config)
}

impl EnrollServer {
    /// Serves `Enrollment` with `cert` on 127.0.0.1 (port 0).
    pub async fn start(ca: Arc<TestCa>, cert: &ServerCert, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor = TlsAcceptor::from(server_config(cert));
        let (tx, rx) = mpsc::channel::<Result<Io, std::io::Error>>(8);
        let handshake_errors = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::clone(&handshake_errors);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    continue;
                };
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                let errors = Arc::clone(&errors);
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(5), acceptor.accept(tcp)).await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send(Ok(Io(tls))).await;
                        }
                        Ok(Err(e)) => errors
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(e.to_string()),
                        Err(_) => {}
                    }
                });
            }
        });
        let station_id = Uuid::new_v4();
        let behaviour = Arc::new(Mutex::new(behaviour));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let service = EnrollmentServer::new(Service {
            ca,
            station_id,
            behaviour: Arc::clone(&behaviour),
            seen: Arc::clone(&seen),
        });
        let serve = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .serve_with_incoming(service, ReceiverStream::new(rx))
                .await;
        });
        Self {
            addr,
            station_id,
            behaviour,
            seen,
            handshake_errors,
            tasks: vec![accept, serve],
        }
    }

    /// `https://localhost:<port>`.
    pub fn url(&self) -> String {
        format!("https://localhost:{}", self.addr.port())
    }

    pub fn set(&self, behaviour: Behaviour) {
        *self
            .behaviour
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = behaviour;
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn handshake_errors(&self) -> Vec<String> {
        self.handshake_errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A dev `stationd.toml` for enrollment against `enroll_url` with the fake
/// backend; returns its path.
pub fn write_config(dir: &Path, enroll_url: &str, root: &Path) -> PathBuf {
    let text = format!(
        "state_dir = {:?}\n[endpoints]\nenroll_url = {enroll_url:?}\n\
         station_api_url = {enroll_url:?}\nroot_ca = {root:?}\n[backend]\nkind = \"fake\"\n",
        dir.join("state"),
    );
    let path = dir.join("stationd.toml");
    std::fs::write(&path, text).expect("write config");
    path
}

/// A well-formed token.
pub const TOKEN: &str = "chs_enr_7K3M9Q2XA4B5C6D7E8F9G0H1J2K3M4N5";
