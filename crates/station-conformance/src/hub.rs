// SPDX-License-Identifier: MIT
//! [`FakeHub`]: a real mTLS gRPC `StationGateway` server on loopback whose
//! hub side is scripted by the test through [`StationConn`].
//!
//! - TLS 1.3 only by default ([`TlsVersions`]), ALPN `h2`, client
//!   certificates required and verified against the configured CA; the
//!   station ID is read from the certificate
//!   (`station_proto::identity`).
//! - Every message the station sends runs through the hub-side validator
//!   (`station_proto::validate::Session`, station-protocol §5): a violation
//!   fails the script with [`ConformanceError::Violation`], drops are
//!   recorded ([`StationConn::drops`]).
//! - Everything is recorded ([`FakeHub::records`]): TLS accepts and
//!   failures, streams, every message in both directions.
//! - The server can be stopped (all connections killed, listener closed) and
//!   restarted on the same port; single connections can be killed or frozen.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use station_proto::v1::station_gateway_server::{StationGateway, StationGatewayServer};
use station_proto::validate::{DropReason, Session, Verdict, check_message, negotiate};
use station_proto::{identity, limits, v1};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

use crate::error::{ConformanceError, Result};
use crate::io::{ConnInfo, KillableIo};

use v1::hub_message::Body as HubBody;
use v1::request::Command;
use v1::station_message::Body;

/// Protocol versions the fake hub's TLS accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsVersions {
    /// TLS 1.3 only (the production gateway).
    Tls13Only,
    /// TLS 1.2 only: a gateway a station must refuse.
    Tls12Only,
}

/// The fake hub's TLS material.
#[derive(Clone)]
pub struct HubTls {
    /// Server certificate chain (PEM, leaf first).
    pub cert_chain_pem: String,
    /// Server private key (PEM).
    pub key_pem: String,
    /// CA bundle (PEM) that station certificates must chain to.
    pub client_ca_pem: String,
    /// Accepted TLS versions.
    pub versions: TlsVersions,
}

impl fmt::Debug for HubTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubTls")
            .field("versions", &self.versions)
            .finish_non_exhaustive()
    }
}

fn hub_err(e: impl fmt::Display) -> ConformanceError {
    ConformanceError::Hub(e.to_string())
}

impl HubTls {
    /// The rustls server configuration (ring provider, mTLS required,
    /// ALPN h2).
    pub fn server_config(&self) -> Result<Arc<ServerConfig>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let chain = CertificateDer::pem_slice_iter(self.cert_chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(hub_err)?;
        let key = PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).map_err(hub_err)?;
        let mut roots = RootCertStore::empty();
        for ca in CertificateDer::pem_slice_iter(self.client_ca_pem.as_bytes()) {
            roots.add(ca.map_err(hub_err)?).map_err(hub_err)?;
        }
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                .build()
                .map_err(hub_err)?;
        let versions: &[&rustls::SupportedProtocolVersion] = match self.versions {
            TlsVersions::Tls13Only => &[&rustls::version::TLS13],
            TlsVersions::Tls12Only => &[&rustls::version::TLS12],
        };
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .map_err(hub_err)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(hub_err)?;
        config.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Arc::new(config))
    }
}

/// What happened, in order ([`FakeHub::records`]).
#[derive(Debug, Clone)]
pub enum RecordEvent {
    /// A TLS handshake completed.
    TlsAccepted {
        /// The station's address.
        remote: Option<SocketAddr>,
    },
    /// A TLS handshake failed.
    TlsFailed {
        /// The error.
        error: String,
    },
    /// The station opened `StationGateway.Connect`.
    StreamOpened {
        /// The station ID from the client certificate.
        station_id: Option<Uuid>,
    },
    /// A message from the station.
    FromStation(v1::StationMessage),
    /// A message to the station.
    ToStation(v1::HubMessage),
    /// The validator dropped a station message (§5).
    Dropped(DropReason),
    /// The station's side of the stream ended.
    StreamEnded {
        /// `None` for a clean end, else the error.
        error: Option<String>,
    },
    /// The script killed the connection.
    Killed,
    /// The script froze the connection.
    Frozen,
}

/// One recorded event.
#[derive(Debug, Clone)]
pub struct Record {
    /// Time since the hub was created.
    pub at: Duration,
    /// Connection number (0 before a connection is identified).
    pub conn: u64,
    /// What happened.
    pub event: RecordEvent,
}

#[derive(Debug)]
struct Shared {
    epoch: Instant,
    next_conn: AtomicU64,
    records: Mutex<Vec<Record>>,
    tls_failures: Mutex<VecDeque<String>>,
    tls_failure: Notify,
}

impl Shared {
    fn records(&self) -> MutexGuard<'_, Vec<Record>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, conn: u64, event: RecordEvent) {
        let at = self.epoch.elapsed();
        self.records().push(Record { at, conn, event });
    }

    fn tls_failed(&self, error: String) {
        self.record(
            0,
            RecordEvent::TlsFailed {
                error: error.clone(),
            },
        );
        self.tls_failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(error);
        self.tls_failure.notify_one();
    }
}

struct Gateway {
    shared: Arc<Shared>,
    conns: mpsc::UnboundedSender<StationConn>,
}

type HubStream = ReceiverStream<Result<v1::HubMessage, Status>>;

#[tonic::async_trait]
impl StationGateway for Gateway {
    type ConnectStream = HubStream;

    async fn connect(
        &self,
        request: Request<Streaming<v1::StationMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let info = request.extensions().get::<ConnInfo>().cloned();
        let station_id = info
            .as_ref()
            .and_then(|i| i.peer_certs.first())
            .and_then(|leaf| identity::station_id_from_cert_der(leaf).ok());
        let conn_id = info.as_ref().map_or(0, |i| i.id);
        self.shared
            .record(conn_id, RecordEvent::StreamOpened { station_id });
        let (tx, rx) = mpsc::channel(64);
        let conn = StationConn {
            id: conn_id,
            info,
            station_id,
            inbound: request.into_inner(),
            outbound: Some(tx),
            session: Session::new(Instant::now().into_std()),
            raw_ids: HashSet::new(),
            next_request_id: limits::FIRST_REQUEST_ID,
            responses: HashMap::new(),
            backlog: VecDeque::new(),
            hello: None,
            closed: false,
            drops: Vec::new(),
            shared: Arc::clone(&self.shared),
        };
        self.conns
            .send(conn)
            .map_err(|_| Status::unavailable("fake hub is shutting down"))?;
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

struct Running {
    cancel: CancellationToken,
    accept: JoinHandle<()>,
    serve: JoinHandle<()>,
}

/// A scripted fake hub gateway; see the [module docs](self).
pub struct FakeHub {
    tls: Arc<ServerConfig>,
    addr: SocketAddr,
    shared: Arc<Shared>,
    conns_tx: mpsc::UnboundedSender<StationConn>,
    conns_rx: mpsc::UnboundedReceiver<StationConn>,
    running: Option<Running>,
}

impl fmt::Debug for FakeHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeHub")
            .field("addr", &self.addr)
            .field("running", &self.running.is_some())
            .finish_non_exhaustive()
    }
}

impl FakeHub {
    /// Starts a hub on `127.0.0.1` with a free port.
    pub async fn start(tls: &HubTls) -> Result<Self> {
        Self::start_on(SocketAddr::from(([127, 0, 0, 1], 0)), tls).await
    }

    /// Starts a hub on `addr`.
    pub async fn start_on(addr: SocketAddr, tls: &HubTls) -> Result<Self> {
        let (conns_tx, conns_rx) = mpsc::unbounded_channel();
        let mut hub = Self {
            tls: tls.server_config()?,
            addr,
            shared: Arc::new(Shared {
                epoch: Instant::now(),
                next_conn: AtomicU64::new(1),
                records: Mutex::new(Vec::new()),
                tls_failures: Mutex::new(VecDeque::new()),
                tls_failure: Notify::new(),
            }),
            conns_tx,
            conns_rx,
            running: None,
        };
        hub.launch().await?;
        Ok(hub)
    }

    /// The listening address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `https://localhost:<port>`, the station's `gateway_url`.
    pub fn gateway_url(&self) -> String {
        format!("https://localhost:{}", self.addr.port())
    }

    /// Whether the server is running.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    async fn launch(&mut self) -> Result<()> {
        // Rebinding right after a stop can race the old socket's teardown.
        let mut attempts = 0;
        let listener = loop {
            match TcpListener::bind(self.addr).await {
                Ok(l) => break l,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempts < 50 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(hub_err(format!("cannot bind {}: {e}", self.addr))),
            }
        };
        self.addr = listener.local_addr().map_err(hub_err)?;
        let cancel = CancellationToken::new();
        let (io_tx, io_rx) = mpsc::channel::<Result<KillableIo, std::io::Error>>(16);
        let accept = tokio::spawn(accept_loop(
            listener,
            TlsAcceptor::from(Arc::clone(&self.tls)),
            io_tx,
            cancel.clone(),
            Arc::clone(&self.shared),
        ));
        let service = StationGatewayServer::new(Gateway {
            shared: Arc::clone(&self.shared),
            conns: self.conns_tx.clone(),
        })
        .max_decoding_message_size(limits::MAX_MESSAGE_BYTES);
        let shutdown = cancel.clone().cancelled_owned();
        let serve = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .serve_with_incoming_shutdown(service, ReceiverStream::new(io_rx), shutdown)
                .await;
        });
        self.running = Some(Running {
            cancel,
            accept,
            serve,
        });
        Ok(())
    }

    /// Stops the server: kills every connection and closes the listener
    /// (a crashed or restarting hub). Idempotent.
    pub async fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.cancel.cancel();
            let _ = r.accept.await;
            let _ = tokio::time::timeout(Duration::from_secs(5), r.serve).await;
        }
    }

    /// Starts the server again on the same port (stopping it first if it
    /// is running).
    pub async fn restart(&mut self) -> Result<()> {
        self.stop().await;
        self.launch().await
    }

    /// The next `Connect` stream opened by a station.
    pub async fn accept(&mut self, within: Duration) -> Result<StationConn> {
        match tokio::time::timeout(within, self.conns_rx.recv()).await {
            Ok(Some(conn)) => Ok(conn),
            Ok(None) => Err(hub_err("connection channel closed")),
            Err(_) => Err(ConformanceError::Timeout {
                what: "the station to open a Connect stream".into(),
                after: within,
            }),
        }
    }

    /// Fails if a station opens a `Connect` stream within `window`.
    pub async fn expect_no_connection(&mut self, window: Duration) -> Result<()> {
        match tokio::time::timeout(window, self.conns_rx.recv()).await {
            Ok(Some(conn)) => Err(ConformanceError::Assertion(format!(
                "the station opened a Connect stream (connection {}) when it must not",
                conn.id
            ))),
            Ok(None) | Err(_) => Ok(()),
        }
    }

    /// The next failed TLS handshake (oldest first).
    pub async fn next_tls_failure(&self, within: Duration) -> Result<String> {
        let deadline = Instant::now() + within;
        loop {
            let notified = self.shared.tls_failure.notified();
            if let Some(e) = self
                .shared
                .tls_failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
            {
                return Ok(e);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(ConformanceError::Timeout {
                    what: "a failed TLS handshake".into(),
                    after: within,
                });
            }
        }
    }

    /// Everything recorded so far.
    pub fn records(&self) -> Vec<Record> {
        self.shared.records().clone()
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        if let Some(r) = &self.running {
            r.cancel.cancel();
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    io_tx: mpsc::Sender<Result<KillableIo, std::io::Error>>,
    cancel: CancellationToken,
    shared: Arc<Shared>,
) {
    loop {
        let (tcp, remote) = tokio::select! {
            () = cancel.cancelled() => break,
            r = listener.accept() => match r {
                Ok(pair) => pair,
                Err(_) => continue,
            },
        };
        let acceptor = acceptor.clone();
        let io_tx = io_tx.clone();
        let kill = cancel.child_token();
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let handshake = tokio::time::timeout(Duration::from_secs(5), acceptor.accept(tcp));
            let tls = tokio::select! {
                () = kill.cancelled() => return,
                r = handshake => match r {
                    Ok(Ok(tls)) => tls,
                    Ok(Err(e)) => return shared.tls_failed(e.to_string()),
                    Err(_) => return shared.tls_failed("TLS handshake timed out".into()),
                },
            };
            let id = shared.next_conn.fetch_add(1, Ordering::Relaxed);
            let peer_certs = tls
                .get_ref()
                .1
                .peer_certificates()
                .map(<[CertificateDer<'_>]>::to_vec)
                .unwrap_or_default()
                .into_iter()
                .map(CertificateDer::into_owned)
                .collect();
            shared.record(
                id,
                RecordEvent::TlsAccepted {
                    remote: Some(remote),
                },
            );
            let info = ConnInfo {
                id,
                remote_addr: Some(remote),
                peer_certs,
                kill,
                frozen: Arc::new(AtomicBool::new(false)),
            };
            let _ = io_tx.send(Ok(KillableIo::new(tls, info))).await;
        });
    }
}

/// One `Connect` stream, scripted from the hub side.
///
/// Requests get increasing IDs from 1. [`StationConn::request_raw`] sends a
/// request the validator does not track (e.g. without a command, as a newer
/// hub's unknown command decodes); its response is only checked
/// statelessly.
pub struct StationConn {
    id: u64,
    info: Option<ConnInfo>,
    station_id: Option<Uuid>,
    inbound: Streaming<v1::StationMessage>,
    outbound: Option<mpsc::Sender<Result<v1::HubMessage, Status>>>,
    session: Session,
    raw_ids: HashSet<u64>,
    next_request_id: u64,
    responses: HashMap<u64, v1::Response>,
    backlog: VecDeque<(Instant, v1::StationMessage)>,
    hello: Option<v1::Hello>,
    closed: bool,
    drops: Vec<DropReason>,
    shared: Arc<Shared>,
}

impl fmt::Debug for StationConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StationConn")
            .field("id", &self.id)
            .field("station_id", &self.station_id)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

fn describe(msg: &v1::StationMessage) -> String {
    let mut s = format!("{msg:?}");
    if s.len() > 300 {
        let mut end = 300;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

impl StationConn {
    /// Connection number within the hub.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The station ID from the client certificate.
    pub fn station_id(&self) -> Option<Uuid> {
        self.station_id
    }

    /// The station's `Hello`, once received.
    pub fn hello(&self) -> Option<&v1::Hello> {
        self.hello.as_ref()
    }

    /// Messages the validator dropped (rate limits etc., §5).
    pub fn drops(&self) -> &[DropReason] {
        &self.drops
    }

    /// Whether the station's side of the stream has ended.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn validate(&mut self, msg: &v1::StationMessage, now: Instant) -> Result<()> {
        let raw_response = match &msg.body {
            Some(Body::Response(r)) => self.raw_ids.remove(&r.request_id),
            _ => false,
        };
        let verdict = if raw_response {
            let minor = self.session.negotiated_version().map_or(0, |v| v.minor);
            match check_message(msg, minor) {
                Ok(()) => Verdict::Accept,
                Err(kind) => Verdict::Violation(kind),
            }
        } else {
            self.session.check(msg, now.into_std())
        };
        match verdict {
            Verdict::Accept => Ok(()),
            Verdict::Drop(reason) => {
                self.shared.record(self.id, RecordEvent::Dropped(reason));
                self.drops.push(reason);
                Ok(())
            }
            Verdict::Violation(kind) => Err(ConformanceError::Violation {
                kind,
                message: describe(msg),
            }),
        }
    }

    /// Reads one message from the stream (not the backlog), validating it.
    async fn read(
        &mut self,
        deadline: Instant,
        what: &str,
    ) -> Result<(Instant, v1::StationMessage)> {
        if self.closed {
            return Err(ConformanceError::StreamClosed { what: what.into() });
        }
        let after = deadline.saturating_duration_since(Instant::now());
        let msg = match tokio::time::timeout_at(deadline, self.inbound.message()).await {
            Err(_) => {
                return Err(ConformanceError::Timeout {
                    what: what.into(),
                    after,
                });
            }
            Ok(Err(status)) => {
                self.closed = true;
                self.shared.record(
                    self.id,
                    RecordEvent::StreamEnded {
                        error: Some(status.to_string()),
                    },
                );
                return Err(ConformanceError::StreamError {
                    what: what.into(),
                    status: status.to_string(),
                });
            }
            Ok(Ok(None)) => {
                self.closed = true;
                self.shared
                    .record(self.id, RecordEvent::StreamEnded { error: None });
                return Err(ConformanceError::StreamClosed { what: what.into() });
            }
            Ok(Ok(Some(msg))) => msg,
        };
        let now = Instant::now();
        self.shared
            .record(self.id, RecordEvent::FromStation(msg.clone()));
        self.validate(&msg, now)?;
        if let Some(Body::Hello(h)) = &msg.body {
            self.hello = Some(h.clone());
        }
        Ok((now, msg))
    }

    /// The next message (backlog first), validated.
    pub async fn recv(&mut self, within: Duration) -> Result<v1::StationMessage> {
        if let Some((_, msg)) = self.backlog.pop_front() {
            return Ok(msg);
        }
        let (_, msg) = self
            .read(Instant::now() + within, "a station message")
            .await?;
        Ok(msg)
    }

    /// Expects `Hello` as the next message.
    pub async fn expect_hello(&mut self, within: Duration) -> Result<v1::Hello> {
        let msg = self.recv(within).await?;
        match msg.body {
            Some(Body::Hello(h)) => Ok(h),
            _ => Err(ConformanceError::Unexpected {
                expected: "Hello".into(),
                got: describe(&msg),
            }),
        }
    }

    /// Sends `msg` to the station.
    pub async fn send(&mut self, msg: v1::HubMessage) -> Result<()> {
        let tx = self
            .outbound
            .as_ref()
            .ok_or_else(|| hub_err("the hub side of the stream is already closed"))?;
        self.shared
            .record(self.id, RecordEvent::ToStation(msg.clone()));
        tx.send(Ok(msg))
            .await
            .map_err(|_| hub_err("the stream to the station is gone"))
    }

    /// Answers the station's `Hello` with a `Welcome` (version from
    /// `station_proto::validate::negotiate`).
    pub async fn welcome(&mut self, status_interval_ms: u32) -> Result<v1::Welcome> {
        let hello = self
            .hello
            .clone()
            .ok_or_else(|| hub_err("welcome() before a Hello was received"))?;
        let version = negotiate(&hello).map_err(hub_err)?;
        let welcome = v1::Welcome {
            protocol_version: Some(version),
            connection_id: format!("conformance-{}", self.id),
            status_interval_ms,
            max_message_bytes: u32::try_from(limits::MAX_MESSAGE_BYTES).unwrap_or(u32::MAX),
            hub_time_unix_ms: 0,
        };
        self.send_welcome(welcome.clone()).await?;
        Ok(welcome)
    }

    /// Sends a specific `Welcome` (e.g. an unsupported version).
    pub async fn send_welcome(&mut self, welcome: v1::Welcome) -> Result<()> {
        self.session
            .on_welcome_sent(&welcome, Instant::now().into_std());
        self.send(v1::HubMessage {
            body: Some(HubBody::Welcome(welcome)),
        })
        .await
    }

    /// Sends a validated request; returns its ID.
    pub async fn request(&mut self, command: Command) -> Result<u64> {
        let request = v1::Request {
            request_id: self.next_request_id,
            command: Some(command),
        };
        self.session
            .on_request_sent(&request, Instant::now().into_std())
            .map_err(|e| hub_err(format!("script sent an invalid request: {e}")))?;
        self.next_request_id += 1;
        let id = request.request_id;
        self.send(v1::HubMessage {
            body: Some(HubBody::Request(request)),
        })
        .await?;
        Ok(id)
    }

    /// Sends a request the validator does not track (its command may be
    /// `None`, as an unknown command from a newer hub decodes); returns its
    /// ID.
    pub async fn request_raw(&mut self, command: Option<Command>) -> Result<u64> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.raw_ids.insert(id);
        self.send(v1::HubMessage {
            body: Some(HubBody::Request(v1::Request {
                request_id: id,
                command,
            })),
        })
        .await?;
        Ok(id)
    }

    /// Waits for the response to request `id`; other messages are kept for
    /// later [`recv`](Self::recv) calls.
    pub async fn expect_response(&mut self, id: u64, within: Duration) -> Result<v1::Response> {
        if let Some(r) = self.responses.remove(&id) {
            return Ok(r);
        }
        let deadline = Instant::now() + within;
        let what = format!("the response to request {id}");
        loop {
            let (at, msg) = self.read(deadline, &what).await?;
            match msg.body {
                Some(Body::Response(r)) if r.request_id == id => return Ok(r),
                Some(Body::Response(r)) => {
                    self.responses.insert(r.request_id, r);
                }
                _ => self.backlog.push_back((at, msg)),
            }
        }
    }

    /// Sends `command` and waits for its response.
    pub async fn call(&mut self, command: Command, within: Duration) -> Result<v1::Response> {
        let id = self.request(command).await?;
        self.expect_response(id, within).await
    }

    /// Every message (backlog first) received during `window`, with its
    /// arrival time. The stream ending is an error.
    pub async fn collect(
        &mut self,
        window: Duration,
    ) -> Result<Vec<(Instant, v1::StationMessage)>> {
        let deadline = Instant::now() + window;
        let mut out: Vec<_> = self.backlog.drain(..).collect();
        loop {
            match self.read(deadline, "messages").await {
                Ok(m) => out.push(m),
                Err(ConformanceError::Timeout { .. }) => return Ok(out),
                Err(e) => return Err(e),
            }
        }
    }

    /// Waits for the station to end its side of the stream; returns what it
    /// sent before.
    pub async fn expect_closed(&mut self, within: Duration) -> Result<Vec<v1::StationMessage>> {
        let deadline = Instant::now() + within;
        let mut out: Vec<_> = self.backlog.drain(..).map(|(_, m)| m).collect();
        loop {
            match self.read(deadline, "the station to close the stream").await {
                Ok((_, m)) => out.push(m),
                Err(
                    ConformanceError::StreamClosed { .. } | ConformanceError::StreamError { .. },
                ) => {
                    return Ok(out);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends `Goodbye` and closes the hub side of the stream.
    pub async fn goodbye(
        &mut self,
        reason: v1::goodbye::Reason,
        retry_after_ms: u32,
    ) -> Result<()> {
        self.send(v1::HubMessage {
            body: Some(HubBody::Goodbye(v1::Goodbye {
                reason: reason as i32,
                message: format!("conformance: {}", reason.as_str_name()),
                retry_after_ms,
                refresh_assignment: false,
            })),
        })
        .await?;
        self.close();
        Ok(())
    }

    /// Ends the hub side of the stream cleanly (gRPC status OK).
    pub fn close(&mut self) {
        self.outbound = None;
    }

    /// Kills the underlying TCP connection abruptly.
    pub fn kill(&self) {
        if let Some(info) = &self.info {
            self.shared.record(self.id, RecordEvent::Killed);
            info.kill();
        }
    }

    /// Stops answering at the transport level (not even HTTP/2 pings): a
    /// dead peer the station must detect by itself.
    pub fn freeze(&self) {
        if let Some(info) = &self.info {
            self.shared.record(self.id, RecordEvent::Frozen);
            info.freeze();
        }
    }
}
