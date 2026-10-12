// SPDX-License-Identifier: GPL-3.0-only
//! No enrollment token in any log line, at TRACE level, of every crate.
//!
//! A test binary of its own: the subscriber that captures the logs is this
//! thread's, and `tracing` caches whether a call site is enabled across
//! threads, so other tests reaching the same call sites at the same time
//! could leave them disabled for it.

mod enroll_support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use enroll_support::{Behaviour, EnrollServer, TOKEN, TestCa, write_config};
use stationd::config::Config;
use stationd::enroll::{self, EnrollOptions, Token};
use tonic::Status;

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
    let options = |force| EnrollOptions {
        force,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
    };
    let token = || Token::parse(TOKEN).expect("token");
    rt.block_on(async {
        let ca = Arc::new(TestCa::new());
        let cert = ca.server_cert(&["localhost"]);
        let server = EnrollServer::start(
            Arc::clone(&ca),
            &cert,
            Behaviour::Sign {
                gateway_url: "https://localhost:7443".to_owned(),
            },
        )
        .await;
        let dir = tempfile::tempdir().expect("tmp");
        let root = ca.write_root(dir.path());
        let config = Config::load(&write_config(dir.path(), &server.url(), &root)).expect("config");
        enroll::enroll(&config, token(), options(false))
            .await
            .expect("enrolled");
        server.set(Behaviour::Fail(Status::unauthenticated("token expired")));
        enroll::enroll(&config, token(), options(true))
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
