// SPDX-License-Identifier: MIT
//! Self-tests of the conformance harness: it must fail stations that do not
//! conform, and its validator must catch protocol violations.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use station_conformance::scenarios::{self, ScenarioConfig};
use station_conformance::{
    BoxError, ConformanceError, Exit, FakeHub, RecordEvent, StationUnderTest, Target, TestPki,
};
use station_proto::v1;
use station_proto::v1::station_gateway_client::StationGatewayClient;
use station_proto::validate::ViolationKind;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use v1::station_message::Body;

/// A "station" that never does anything.
struct Dead;

#[async_trait::async_trait]
impl StationUnderTest for Dead {
    async fn start(&mut self, _target: &Target) -> Result<(), BoxError> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<Exit, BoxError> {
        Ok(Exit {
            success: true,
            detail: "never ran".into(),
        })
    }
    async fn wait_exit(&mut self, _within: Duration) -> Result<Option<Exit>, BoxError> {
        Ok(None)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_station_fails_every_scenario() {
    let cfg = ScenarioConfig {
        connect_within: Duration::from_millis(300),
        reconnect_within: Duration::from_millis(300),
        response_within: Duration::from_millis(300),
        quiet_period: Duration::from_millis(100),
        exit_within: Duration::from_millis(300),
        dead_peer_within: Some(Duration::from_millis(300)),
        ..ScenarioConfig::fast()
    };
    let runs = scenarios::ALL.iter().map(|s| {
        let cfg = cfg.clone();
        async move {
            let mut dead = Dead;
            ((s.run)(&mut dead, &cfg).await, s.name)
        }
    });
    for (result, name) in futures_util::future::join_all(runs).await {
        let e = result.expect_err(name);
        assert!(
            matches!(e, ConformanceError::Timeout { .. }),
            "{name}: expected a timeout, got {e}"
        );
    }
    assert!(scenarios::by_name("answers_ping").is_some());
    assert!(scenarios::by_name("nope").is_none());
    assert!(format!("{:?}", scenarios::ALL[0]).contains("hello_then_welcome"));
}

#[derive(Clone)]
struct RawConnector {
    tls: Arc<rustls::ClientConfig>,
    port: u16,
}

impl tower_service::Service<tonic::transport::Uri> for RawConnector {
    type Response = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: tonic::transport::Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let tcp = TcpStream::connect(("127.0.0.1", this.port)).await?;
            let name = ServerName::try_from("localhost").map_err(std::io::Error::other)?;
            let tls = tokio_rustls::TlsConnector::from(this.tls)
                .connect(name, tcp)
                .await?;
            Ok(TokioIo::new(tls))
        })
    }
}

/// A hand-written client that sends whatever the test tells it to.
async fn raw_station(pki: &TestPki, hub: &FakeHub) -> mpsc::Sender<v1::StationMessage> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(pki.ca_pem.as_bytes()).expect("ca"))
        .expect("root");
    let chain =
        vec![CertificateDer::from_pem_slice(pki.station_cert_pem.as_bytes()).expect("cert")];
    let key = PrivateKeyDer::from_pem_slice(pki.station_key_pem.as_bytes()).expect("key");
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_root_certificates(roots)
    .with_client_auth_cert(chain, key)
    .expect("client auth");
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(RawConnector {
            tls: Arc::new(tls),
            port: hub.addr().port(),
        })
        .await
        .expect("channel");
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut client = StationGatewayClient::new(channel);
        if let Ok(response) = client.connect(ReceiverStream::new(rx)).await {
            let mut inbound = response.into_inner();
            while let Ok(Some(_)) = inbound.message().await {}
        }
    });
    tx
}

fn msg(body: Body) -> v1::StationMessage {
    v1::StationMessage { body: Some(body) }
}

fn hello() -> v1::StationMessage {
    msg(Body::Hello(v1::Hello {
        protocol_version: Some(station_proto::PROTOCOL_VERSION),
        daemon_version: "raw".into(),
        backend: Some(v1::BackendInfo {
            kind: v1::BackendKind::Ka9qRadio as i32,
            version: String::new(),
        }),
        platform: "test".into(),
        boot_id: 1,
    }))
}

const WAIT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn first_message_must_be_hello() {
    let pki = TestPki::generate().expect("pki");
    let mut hub = FakeHub::start(&pki.hub_tls()).await.expect("hub");
    let tx = raw_station(&pki, &hub).await;
    tx.send(msg(Body::Audio(v1::AudioFrame::default())))
        .await
        .expect("send");
    let mut conn = hub.accept(WAIT).await.expect("accept");
    assert_eq!(conn.station_id(), Some(pki.station_id));
    let e = conn.expect_hello(WAIT).await.expect_err("violation");
    assert!(
        matches!(
            e,
            ConformanceError::Violation {
                kind: ViolationKind::FirstMessageNotHello { .. },
                ..
            }
        ),
        "{e}"
    );
    let kinds: Vec<_> = hub.records().into_iter().map(|r| r.event).collect();
    assert!(
        kinds
            .iter()
            .any(|e| matches!(e, RecordEvent::TlsAccepted { .. }))
    );
    assert!(kinds.iter().any(|e| matches!(
        e,
        RecordEvent::StreamOpened {
            station_id: Some(_)
        }
    )));
    assert!(
        kinds
            .iter()
            .any(|e| matches!(e, RecordEvent::FromStation(_)))
    );
}

#[tokio::test]
async fn unsolicited_and_duplicate_messages_are_violations() {
    let pki = TestPki::generate().expect("pki");
    let mut hub = FakeHub::start(&pki.hub_tls()).await.expect("hub");
    let tx = raw_station(&pki, &hub).await;
    tx.send(hello()).await.expect("send");
    let mut conn = hub.accept(WAIT).await.expect("accept");
    conn.expect_hello(WAIT).await.expect("hello");
    assert!(conn.hello().is_some());
    conn.welcome(1000).await.expect("welcome");
    let id = conn
        .request(v1::request::Command::Ping(v1::Ping::default()))
        .await
        .expect("ping");
    // Answer with the wrong id: unsolicited.
    tx.send(msg(Body::Response(v1::Response {
        request_id: id + 100,
        result: Some(v1::Result {
            code: v1::ResultCode::Ok as i32,
            message: String::new(),
        }),
        payload: None,
    })))
    .await
    .expect("send");
    let e = conn.expect_response(id, WAIT).await.expect_err("violation");
    assert!(
        matches!(
            e,
            ConformanceError::Violation {
                kind: ViolationKind::UnsolicitedResponse { .. },
                ..
            }
        ),
        "{e}"
    );
    // A second Hello on a fresh stream after Welcome.
    let tx2 = raw_station(&pki, &hub).await;
    tx2.send(hello()).await.expect("send");
    tx2.send(hello()).await.expect("send");
    let mut conn2 = hub.accept(WAIT).await.expect("accept");
    conn2.expect_hello(WAIT).await.expect("hello");
    let e = conn2.recv(WAIT).await.expect_err("duplicate");
    assert!(
        matches!(
            e,
            ConformanceError::Violation {
                kind: ViolationKind::DuplicateHello,
                ..
            }
        ),
        "{e}"
    );
    // Script misuse is reported as a hub error.
    let mut conn3 = {
        let tx3 = raw_station(&pki, &hub).await;
        tx3.send(hello()).await.expect("send");
        drop(tx3);
        hub.accept(WAIT).await.expect("accept")
    };
    let e = conn3.welcome(1000).await.expect_err("no hello yet");
    assert!(matches!(e, ConformanceError::Hub(_)), "{e}");
    let e = conn3
        .request(v1::request::Command::Ping(v1::Ping::default()))
        .await
        .expect_err("before welcome");
    assert!(matches!(e, ConformanceError::Hub(_)), "{e}");
    conn3.expect_hello(WAIT).await.expect("hello");
    // The station ended its side: recv reports the closed stream.
    let e = conn3.recv(WAIT).await.expect_err("closed");
    assert!(matches!(e, ConformanceError::StreamClosed { .. }), "{e}");
    assert!(conn3.is_closed());
    let e = conn3.recv(WAIT).await.expect_err("still closed");
    assert!(matches!(e, ConformanceError::StreamClosed { .. }), "{e}");
    conn3.close();
    let e = conn3
        .goodbye(v1::goodbye::Reason::Shutdown, 0)
        .await
        .expect_err("hub side closed");
    assert!(matches!(e, ConformanceError::Hub(_)), "{e}");
    assert!(format!("{conn3:?}").contains("StationConn"));
    assert!(conn3.drops().is_empty());
}

#[tokio::test]
async fn stop_restart_and_timeouts() {
    let pki = TestPki::generate().expect("pki");
    let mut hub = FakeHub::start(&pki.hub_tls()).await.expect("hub");
    assert!(hub.is_running());
    let port = hub.addr().port();
    assert_eq!(hub.gateway_url(), format!("https://localhost:{port}"));
    hub.stop().await;
    hub.stop().await;
    assert!(!hub.is_running());
    hub.restart().await.expect("restart");
    assert_eq!(hub.addr().port(), port, "same port");
    assert!(format!("{hub:?}").contains("running: true"));
    let e = hub
        .accept(Duration::from_millis(50))
        .await
        .expect_err("nobody");
    assert!(matches!(e, ConformanceError::Timeout { .. }), "{e}");
    hub.expect_no_connection(Duration::from_millis(50))
        .await
        .expect("quiet");
    let e = hub
        .next_tls_failure(Duration::from_millis(50))
        .await
        .expect_err("none");
    assert!(matches!(e, ConformanceError::Timeout { .. }), "{e}");
    // A plain TCP client that never speaks TLS is a handshake failure.
    let tcp = TcpStream::connect(hub.addr()).await.expect("tcp");
    drop(tcp);
    let failure = hub.next_tls_failure(WAIT).await.expect("tls failure");
    assert!(!failure.is_empty());
    // A client connecting while the script expects silence.
    let tx = raw_station(&pki, &hub).await;
    tx.send(hello()).await.expect("send");
    let e = hub.expect_no_connection(WAIT).await.expect_err("connected");
    assert!(matches!(e, ConformanceError::Assertion(_)), "{e}");
    let other = TestPki::generate().expect("pki");
    let bad = station_conformance::HubTls {
        key_pem: other.hub_key_pem.clone(),
        ..pki.hub_tls()
    };
    assert!(
        FakeHub::start(&bad).await.is_err(),
        "key does not match certificate"
    );
    assert!(format!("{bad:?}").contains("Tls13Only"));
    assert!(!format!("{bad:?}").contains("PRIVATE"));
}
