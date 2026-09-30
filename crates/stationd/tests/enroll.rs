// SPDX-License-Identifier: GPL-3.0-only
//! `stationd enroll` against an in-test `Enrollment` server over TLS 1.3 on
//! loopback, with certificates generated in-test: success (and the enrolled
//! station then connects to its assigned hub), refusals, TLS failures,
//! unusable responses, and no token in any log line.
//!
//! These use the library with a development-flavour config, so they also
//! run in packaged builds.

mod common;
mod enroll_support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use enroll_support::{Behaviour, EnrollServer, TOKEN, TestCa, write_config};
use station_backend::FakeBackend;
use stationd::config::Config;
use stationd::credentials;
use stationd::enroll::{self, EnrollError, EnrollOptions, Token, TokenProblem};
use stationd::flavour::Flavour;
use stationd::local_status;
use stationd::state::{self, StateDir};
use tonic::Status;

const WAIT: Duration = Duration::from_secs(10);

fn options(force: bool) -> EnrollOptions {
    EnrollOptions {
        force,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
    }
}

fn token() -> Token {
    Token::parse(TOKEN).expect("token")
}

struct Setup {
    ca: Arc<TestCa>,
    server: EnrollServer,
    dir: tempfile::TempDir,
    config: Config,
}

impl Setup {
    async fn new(behaviour: Behaviour) -> Self {
        let ca = Arc::new(TestCa::new());
        let cert = ca.server_cert(&["localhost"]);
        Self::with_server_cert(ca, &cert, behaviour).await
    }

    async fn with_server_cert(
        ca: Arc<TestCa>,
        cert: &enroll_support::ServerCert,
        behaviour: Behaviour,
    ) -> Self {
        let server = EnrollServer::start(Arc::clone(&ca), cert, behaviour).await;
        let dir = tempfile::tempdir().expect("tmp");
        let root = ca.write_root(dir.path());
        let path = write_config(dir.path(), &server.url(), &root);
        let config = Config::load_as(&path, Flavour::development()).expect("config");
        Self {
            ca,
            server,
            dir,
            config,
        }
    }

    fn state(&self) -> StateDir {
        StateDir::new(&self.config.state_dir)
    }
}

fn sign(url: &str) -> Behaviour {
    Behaviour::Sign {
        gateway_url: url.to_owned(),
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).expect("stat").permissions().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrolls_then_connects_to_the_assigned_hub() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let cert = ca.server_cert(&["localhost"]);
    let s = Setup::with_server_cert(Arc::clone(&ca), &cert, sign(&hub.gateway_url())).await;

    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect("enrolled");
    assert_eq!(e.station_id, s.server.station_id);
    assert_eq!(e.assignment.hub_id, "sa1");
    assert_eq!(e.assignment.gateway_url, hub.gateway_url());
    let seen = s.server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].token, TOKEN);
    assert_eq!(seen[0].daemon_version, env!("CARGO_PKG_VERSION"));
    assert!(seen[0].csr_len > 100);

    // The state directory: 0700, the key 0600, everything parseable.
    let st = s.state();
    assert_eq!(mode(st.path()), 0o700);
    assert_eq!(mode(&st.file(state::KEY_FILE)), 0o600);
    assert_eq!(st.present(), state::CREDENTIAL_FILES.to_vec());
    let chain = std::fs::read_to_string(st.file(state::CHAIN_FILE)).expect("chain");
    assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 1);
    let creds = credentials::load(&s.config).expect("enrolled credentials");
    assert_eq!(creds.tls.station_id, s.server.station_id);
    assert_eq!(creds.hub_id.as_deref(), Some("sa1"));
    // Nothing in the state directory holds the token.
    for f in state::CREDENTIAL_FILES {
        let text = std::fs::read_to_string(st.file(f)).expect("read");
        assert!(!text.contains(&TOKEN[8..]), "{f} holds the token");
    }
    let report = local_status::report(&s.config, local_status::unix_now());
    assert!(report.enrolled);
    assert_eq!(report.hub_id.as_deref(), Some("sa1"));
    assert!(report.cert_expires_in_s.expect("expiry") > 29 * 86_400);

    // The enrolled station connects to the assigned hub with its new
    // certificate (leaf + station intermediate, verified against the root).
    let mut handle = stationd::spawn(
        &s.config,
        Arc::new(FakeBackend::default()),
        common::fast_options(),
    )
    .expect("spawn");
    assert_eq!(handle.station_id(), s.server.station_id);
    assert_eq!(handle.gateway_url(), hub.gateway_url());
    let mut conn = hub.accept(WAIT).await.expect("connects");
    conn.expect_hello(WAIT).await.expect("hello");
    assert_eq!(conn.station_id(), Some(s.server.station_id));
    handle.shutdown();
    drop(conn);
    tokio::time::timeout(WAIT, handle.join())
        .await
        .expect("stops")
        .expect("clean stop");
    drop(s.ca);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_when_enrolled_unless_forced() {
    let s = Setup::new(sign("https://localhost:7443")).await;
    enroll::enroll(&s.config, token(), options(false))
        .await
        .expect("first");
    let key = std::fs::read_to_string(s.state().file(state::KEY_FILE)).expect("key");
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("already enrolled");
    assert!(
        matches!(e, EnrollError::AlreadyEnrolled { station_id } if station_id == s.server.station_id),
        "{e}"
    );
    assert!(e.to_string().contains("--force"), "{e}");
    assert_eq!(s.server.seen().len(), 1, "no request without --force");
    assert!(matches!(
        enroll::preflight(&s.config, false),
        Err(EnrollError::AlreadyEnrolled { .. })
    ));
    enroll::enroll(&s.config, token(), options(true))
        .await
        .expect("forced");
    let new_key = std::fs::read_to_string(s.state().file(state::KEY_FILE)).expect("key");
    assert_ne!(key, new_key, "re-enrollment uses a new key");
    assert!(credentials::load(&s.config).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_refusals_leave_no_state() {
    let s = Setup::new(Behaviour::Fail(Status::unauthenticated("token expired"))).await;
    for (status, want) in [
        ("token expired", TokenProblem::Expired),
        ("token already used", TokenProblem::AlreadyUsed),
        ("invalid token", TokenProblem::Invalid),
    ] {
        s.server
            .set(Behaviour::Fail(Status::unauthenticated(status)));
        let e = enroll::enroll(&s.config, token(), options(false))
            .await
            .expect_err("refused");
        assert!(matches!(e, EnrollError::Token(p) if p == want), "{e}");
        assert!(!e.to_string().contains(&TOKEN[8..]), "{e}");
    }
    s.server.set(Behaviour::Fail(Status::permission_denied(
        "station is killed",
    )));
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("refused");
    assert_eq!(
        e.to_string(),
        "the enrollment service refused the request (PermissionDenied): station is killed"
    );
    assert!(s.state().present().is_empty(), "no credentials written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_from_another_ca_is_refused() {
    let other = TestCa::new();
    let ca = Arc::new(TestCa::new());
    let cert = other.server_cert(&["localhost"]);
    let s = Setup::with_server_cert(ca, &cert, sign("https://localhost:7443")).await;
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("untrusted");
    assert!(matches!(e, EnrollError::Tls { .. }), "{e}");
    let msg = e.to_string();
    assert!(
        msg.contains("UnknownIssuer") || msg.contains("invalid peer certificate"),
        "{msg}"
    );
    assert!(msg.contains("trusts only"), "{msg}");
    assert!(
        s.server.seen().is_empty(),
        "the token never left the station"
    );
    assert!(s.state().present().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_certificate_for_another_name_is_refused() {
    let ca = Arc::new(TestCa::new());
    let cert = ca.server_cert(&["enroll.elsewhere.example"]);
    let s = Setup::with_server_cert(Arc::clone(&ca), &cert, sign("https://localhost:7443")).await;
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("wrong name");
    assert!(matches!(e, EnrollError::Tls { .. }), "{e}");
    let msg = e.to_string();
    assert!(
        msg.contains("NotValidForName") || msg.contains("not valid for name"),
        "{msg}"
    );
    assert!(s.server.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_responses_are_refused() {
    let s = Setup::new(Behaviour::WrongKey {
        gateway_url: "https://localhost:7443".into(),
    })
    .await;
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("wrong key");
    assert!(e.to_string().contains("not for this station's key"), "{e}");
    s.server.set(Behaviour::WrongId {
        gateway_url: "https://localhost:7443".into(),
    });
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("wrong id");
    assert!(e.to_string().contains("another station"), "{e}");
    s.server.set(sign("http://plain.example"));
    let e = enroll::enroll(&s.config, token(), options(false))
        .await
        .expect_err("bad gateway");
    assert!(matches!(e, EnrollError::BadResponse(_)), "{e}");
    assert!(s.state().present().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_service_is_a_network_error() {
    let s = Setup::new(sign("https://localhost:7443")).await;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let root = s.ca.write_root(s.dir.path());
    let path = write_config(s.dir.path(), &format!("https://127.0.0.1:{port}"), &root);
    let config = Config::load_as(&path, Flavour::development()).expect("config");
    let e = enroll::enroll(&config, token(), options(false))
        .await
        .expect_err("closed port");
    assert!(matches!(e, EnrollError::Network { .. }), "{e}");
}

#[test]
fn preflight_rules() {
    let dir = tempfile::tempdir().expect("tmp");
    let p = dir.path().join("stationd.toml");
    std::fs::write(&p, "[sdr]\nprofile = \"rtlsdr\"\n").expect("write");
    let c = Config::load_as(&p, Flavour::development()).expect("config");
    assert!(matches!(
        enroll::preflight(&c, false),
        Err(EnrollError::NoEndpoints)
    ));
    for f in ["c.pem", "k.pem", "ca.pem"] {
        std::fs::write(dir.path().join(f), "x").expect("write");
    }
    std::fs::write(
        &p,
        "[credentials]\ngateway_url = \"https://localhost:1\"\ncert = \"c.pem\"\nkey = \"k.pem\"\n\
         gateway_ca = \"ca.pem\"\n[backend]\nkind = \"fake\"\n",
    )
    .expect("write");
    let c = Config::load_as(&p, Flavour::development()).expect("config");
    let e = enroll::preflight(&c, true).expect_err("static");
    assert!(e.to_string().contains("static [credentials]"), "{e}");
}

/// A `tracing` writer into a shared buffer.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every log line of every crate at TRACE level, for a successful and a
/// refused enrollment, and the token is in none of them.
#[test]
fn no_token_in_any_log_line() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // One thread: every task (client and server) logs to this subscriber.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let s = Setup::new(sign("https://localhost:7443")).await;
        enroll::enroll(&s.config, token(), options(false))
            .await
            .expect("enrolled");
        s.server
            .set(Behaviour::Fail(Status::unauthenticated("token expired")));
        enroll::enroll(&s.config, token(), options(true))
            .await
            .expect_err("refused");
    });
    let log = String::from_utf8_lossy(&captured.0.lock().unwrap_or_else(PoisonError::into_inner))
        .into_owned();
    assert!(log.contains("enrolling this station"), "{log}");
    assert!(log.contains("TRACE"), "trace level was captured");
    assert!(!log.contains(&TOKEN[8..]), "the token reached a log line");
    assert!(!log.contains(&TOKEN[8..].to_ascii_lowercase()));
    assert!(!log.contains("PRIVATE KEY"));
}
