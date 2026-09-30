// SPDX-License-Identifier: GPL-3.0-only
//! `StationControl` against an in-test server over mTLS on loopback, with
//! certificates generated in-test (brief contracts C and D):
//!
//! - the client: `GetAssignment`, `RenewCertificate` and the atomic swap,
//!   refusals, TLS against `gateway-ca.pem` only;
//! - the daemon: the assignment at startup (and the fallback to
//!   `assignment.json`), `Goodbye{refresh_assignment}`, renewal followed by
//!   an immediate reconnect with the new certificate, revocation (exit 3)
//!   and its "newer certificate on disk" exception, clock skew in `status`
//!   and `doctor`.
//!
//! These use the library with development-flavour configs, so they also
//! run in packaged builds.

mod common;
mod enroll_support;

use std::sync::Arc;
use std::time::Duration;

use enroll_support::{Call, ControlServer, Renew, Script, TestCa, time_now, write_enrolled_config};
use station_backend::FakeBackend;
use station_conformance::{FakeHub, StationConn};
use station_proto::v1;
use stationd::config::Config;
use stationd::daemon::{ConnectionState, Observed, StopReason};
use stationd::flavour::Flavour;
use stationd::state::{self, NewCredentials, StateDir};
use stationd::station_api::{ControlError, StationApi};
use stationd::{DaemonHandle, RunError};
use time::OffsetDateTime;
use tokio::sync::watch;
use tonic::Status;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

type Validity = (OffsetDateTime, OffsetDateTime);

/// Issued now (backdated 5 minutes, as the CA does), valid 30 days.
fn fresh() -> Validity {
    let now = time_now();
    (
        now - time::Duration::minutes(5),
        now + time::Duration::days(30),
    )
}

/// Past two thirds of a 30-minute lifetime: due for renewal.
fn due() -> Validity {
    let now = time_now();
    (
        now - time::Duration::minutes(25),
        now + time::Duration::minutes(5),
    )
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the script's assignment is a Result"
)]
fn assign(hub_id: &str, url: &str) -> Result<(String, String), Status> {
    Ok((hub_id.to_owned(), url.to_owned()))
}

fn sign_days(days: i64) -> Renew {
    Renew::Sign {
        lifetime: time::Duration::days(days),
    }
}

struct Env {
    ca: Arc<TestCa>,
    control: ControlServer,
    _dir: tempfile::TempDir,
    config: Config,
    station_id: Uuid,
    serial: String,
}

impl Env {
    /// An enrolled station (certificate `validity`, assigned to
    /// `hub_id` at `gateway_url`) and a `StationControl` server running
    /// `script`.
    async fn new(script: Script, validity: Validity, hub_id: &str, gateway_url: &str) -> Self {
        let ca = Arc::new(TestCa::new());
        Self::with_ca(ca, script, validity, hub_id, gateway_url).await
    }

    async fn with_ca(
        ca: Arc<TestCa>,
        script: Script,
        validity: Validity,
        hub_id: &str,
        gateway_url: &str,
    ) -> Self {
        let cert = ca.server_cert(&["localhost"]);
        let control = ControlServer::start(Arc::clone(&ca), &cert, script).await;
        Self::around(ca, control, validity, hub_id, gateway_url)
    }

    fn around(
        ca: Arc<TestCa>,
        control: ControlServer,
        validity: Validity,
        hub_id: &str,
        gateway_url: &str,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        let root = ca.write_root(dir.path());
        let path = write_enrolled_config(dir.path(), &control.url(), &root);
        let config = Config::load_as(&path, Flavour::development()).expect("config");
        let station_id = Uuid::new_v4();
        let serial = ca.write_enrolled(
            &StateDir::new(&config.state_dir),
            station_id,
            validity,
            hub_id,
            gateway_url,
        );
        Self {
            ca,
            control,
            _dir: dir,
            config,
            station_id,
            serial,
        }
    }

    fn state(&self) -> StateDir {
        StateDir::new(&self.config.state_dir)
    }

    fn api(&self) -> StationApi {
        let mut api = StationApi::from_config(&self.config).expect("enrolled with endpoints");
        api.assignment_timeout = Duration::from_secs(5);
        api.renew_timeout = Duration::from_secs(5);
        api
    }

    fn disk_serial(&self) -> String {
        stationd::credentials::load(&self.config)
            .expect("credentials")
            .tls
            .serial
    }

    fn spawn(&self) -> DaemonHandle {
        stationd::spawn(
            &self.config,
            Arc::new(FakeBackend::default()),
            common::fast_options(),
        )
        .expect("spawn")
    }
}

async fn wait_observed(
    rx: &mut watch::Receiver<Observed>,
    pred: impl FnMut(&Observed) -> bool,
) -> Observed {
    tokio::time::timeout(WAIT, rx.wait_for(pred))
        .await
        .expect("observed in time")
        .expect("daemon alive")
        .clone()
}

async fn wait_state(
    rx: &mut watch::Receiver<ConnectionState>,
    pred: impl FnMut(&ConnectionState) -> bool,
) -> ConnectionState {
    tokio::time::timeout(WAIT, rx.wait_for(pred))
        .await
        .expect("state in time")
        .expect("daemon alive")
        .clone()
}

async fn connected(hub: &mut FakeHub) -> StationConn {
    let mut conn = hub.accept(WAIT).await.expect("station connects");
    conn.expect_hello(WAIT).await.expect("hello");
    conn.welcome(60_000).await.expect("welcome");
    conn
}

async fn stop(mut d: DaemonHandle) {
    d.shutdown();
    tokio::time::timeout(WAIT, d.join())
        .await
        .expect("stops")
        .expect("clean stop");
}

async fn exits_revoked(mut d: DaemonHandle) {
    let result = tokio::time::timeout(WAIT, d.join()).await.expect("exits");
    assert!(matches!(result, Err(RunError::Revoked)), "{result:?}");
    assert_eq!(
        d.current_state(),
        ConnectionState::Stopped(StopReason::Revoked)
    );
}

// ------------------------------------------------------------------ client

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_assignment_stores_changes_only() {
    let e = Env::new(
        Script {
            assignment: assign("eu1", "https://gw.eu1.example:8443"),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        "https://localhost:7443",
    )
    .await;
    let api = e.api();
    let a = api.get_assignment().await.expect("assignment");
    assert_eq!(a.hub_id, "eu1");
    let u = api.refresh_assignment().await.expect("refresh");
    assert!(u.changed);
    assert_eq!(e.state().assignment().expect("stored"), u.assignment);
    assert!(!api.refresh_assignment().await.expect("again").changed);
    // Identity comes from the certificate only.
    let calls = e.control.calls();
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .all(|c| c.station_id == e.station_id && c.serial == e.serial)
    );
    // A malformed assignment is refused and not stored.
    e.control.set(Script {
        assignment: assign("sa 1", "https://x.example"),
        renew: sign_days(30),
    });
    let err = api.refresh_assignment().await.expect_err("malformed");
    assert!(matches!(err, ControlError::BadResponse(_)), "{err}");
    e.control.set(Script {
        assignment: assign("sa1", "http://plain.example"),
        renew: sign_days(30),
    });
    assert!(matches!(
        api.get_assignment().await,
        Err(ControlError::BadResponse(_))
    ));
    assert_eq!(e.state().assignment().expect("kept").hub_id, "eu1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_are_classified() {
    let e = Env::new(
        Script {
            assignment: Err(Status::permission_denied("revoked")),
            renew: Renew::Fail(Status::permission_denied("revoked")),
        },
        fresh(),
        "sa1",
        "https://localhost:7443",
    )
    .await;
    let api = e.api();
    let err = api.get_assignment().await.expect_err("revoked");
    assert!(
        matches!(&err, ControlError::Revoked { serial } if *serial == e.serial),
        "{err}"
    );
    let key = std::fs::read_to_string(e.state().file(state::KEY_FILE)).expect("key");
    let err = api.renew().await.expect_err("revoked");
    assert!(err.is_revoked(), "{err}");
    assert_eq!(
        std::fs::read_to_string(e.state().file(state::KEY_FILE)).expect("key"),
        key,
        "nothing stored"
    );
    for (status, check) in [
        (
            Status::failed_precondition("station is not active"),
            (|e: &ControlError| matches!(e, ControlError::NotActive(_)))
                as fn(&ControlError) -> bool,
        ),
        (Status::permission_denied("other"), |e| {
            matches!(e, ControlError::Refused { .. })
        }),
        (Status::resource_exhausted("try again later"), |e| {
            matches!(e, ControlError::Refused { .. })
        }),
        (Status::invalid_argument("same key"), |e| {
            matches!(e, ControlError::Refused { .. })
        }),
        (Status::unavailable("try again later"), |e| {
            matches!(e, ControlError::Network { .. })
        }),
    ] {
        e.control.set(Script {
            assignment: Err(status.clone()),
            renew: Renew::Fail(status.clone()),
        });
        let a = api.get_assignment().await.expect_err("refused");
        assert!(check(&a), "{status:?}: {a}");
        let r = api.renew().await.expect_err("refused");
        assert!(check(&r), "{status:?}: {r}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_swaps_in_a_new_key_and_certificate() {
    let e = Env::new(
        Script {
            assignment: assign("sa1", "https://localhost:7443"),
            renew: sign_days(30),
        },
        due(),
        "sa1",
        "https://localhost:7443",
    )
    .await;
    let st = e.state();
    let old_key = std::fs::read_to_string(st.file(state::KEY_FILE)).expect("key");
    let old_ca = std::fs::read_to_string(st.file(state::GATEWAY_CA_FILE)).expect("ca");
    let r = e.api().renew().await.expect("renewed");
    assert_eq!(r.previous_serial, e.serial);
    assert_ne!(r.serial, e.serial);
    assert!(!r.ca_rotated);
    assert!(r.validity.not_after_unix_s - r.validity.not_before_unix_s > 29 * 86_400);
    // The new set, as one: a new key, the certificate for it, the chain.
    let creds = stationd::credentials::load(&e.config).expect("loads");
    assert_eq!(creds.tls.serial, r.serial);
    assert_eq!(creds.tls.station_id, e.station_id);
    assert_ne!(
        std::fs::read_to_string(st.file(state::KEY_FILE)).expect("key"),
        old_key
    );
    assert_eq!(
        std::fs::read_to_string(st.file(state::GATEWAY_CA_FILE)).expect("ca"),
        old_ca
    );
    assert!(!st.path().join(state::NEXT_DIR).exists());
    let key_mode = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(st.file(state::KEY_FILE))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(key_mode, 0o600);
    assert_eq!(
        e.control.calls(),
        vec![Call {
            method: "RenewCertificate",
            serial: e.serial.clone(),
            station_id: e.station_id,
        }]
    );
    // The next renewal presents the new certificate and may rotate the CA
    // bundle (it must stay usable: here, the root plus another root).
    let other = TestCa::new();
    let bundle = format!("{}{}", e.ca.root.pem, other.root.pem);
    e.control.set(Script {
        assignment: assign("sa1", "https://localhost:7443"),
        renew: Renew::SignWithBundle {
            lifetime: time::Duration::days(30),
            bundle,
        },
    });
    let r2 = e.api().renew().await.expect("renewed again");
    assert!(r2.ca_rotated);
    assert_eq!(e.control.calls()[1].serial, r.serial);
    let creds = stationd::credentials::load(&e.config).expect("loads");
    assert_eq!(creds.tls.trust_anchors, 2);
    // And station-api is still reachable with the new credentials.
    e.api().get_assignment().await.expect("still trusted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_renewals_keep_the_old_certificate() {
    let e = Env::new(
        Script {
            assignment: assign("sa1", "https://localhost:7443"),
            renew: Renew::OtherStation,
        },
        due(),
        "sa1",
        "https://localhost:7443",
    )
    .await;
    let err = e.api().renew().await.expect_err("other station");
    assert!(err.to_string().contains("another station"), "{err}");
    e.control.set(Script {
        assignment: assign("sa1", "https://localhost:7443"),
        renew: Renew::SignWithBundle {
            lifetime: time::Duration::days(30),
            bundle: "not a certificate".into(),
        },
    });
    let err = e.api().renew().await.expect_err("bad bundle");
    assert!(matches!(err, ControlError::BadResponse(_)), "{err}");
    assert_eq!(e.disk_serial(), e.serial);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn station_api_is_verified_against_the_gateway_ca_only() {
    // A server certificate from another CA: refused before any call.
    let ca = Arc::new(TestCa::new());
    let other = TestCa::new();
    let control = ControlServer::start(
        Arc::clone(&ca),
        &other.server_cert(&["localhost"]),
        Script {
            assignment: assign("sa1", "https://localhost:7443"),
            renew: sign_days(30),
        },
    )
    .await;
    let e = Env::around(ca, control, fresh(), "sa1", "https://localhost:7443");
    let err = e.api().get_assignment().await.expect_err("untrusted");
    assert!(matches!(err, ControlError::Tls { .. }), "{err}");
    assert!(e.control.calls().is_empty());
    // Nothing listening.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let api = StationApi::new(
        stationd::config::Endpoint::parse("x", &format!("https://127.0.0.1:{port}")).expect("url"),
        e.state(),
    );
    assert!(matches!(
        api.get_assignment().await,
        Err(ControlError::Network { .. })
    ));
    // Not enrolled.
    let empty = tempfile::tempdir().expect("tmp");
    let api = StationApi::new(
        stationd::config::Endpoint::parse("x", "https://localhost:1").expect("url"),
        StateDir::new(empty.path()),
    );
    assert!(matches!(
        api.renew().await,
        Err(ControlError::Credentials(_))
    ));
}

// ------------------------------------------------------------------ daemon

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_assignment_moves_the_station_to_its_new_hub() {
    let ca = Arc::new(TestCa::new());
    let mut old_hub = ca.hub().await;
    let mut new_hub = ca.hub().await;
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: assign("eu1", &new_hub.gateway_url()),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        &old_hub.gateway_url(),
    )
    .await;
    let d = e.spawn();
    let mut observed = d.observed();
    let conn = connected(&mut new_hub).await;
    assert_eq!(conn.station_id(), Some(e.station_id));
    let o = wait_observed(&mut observed, |o| o.hub_id.as_deref() == Some("eu1")).await;
    assert_eq!(o.gateway_url, new_hub.gateway_url());
    assert_eq!(o.serial, e.serial);
    assert_eq!(e.state().assignment().expect("stored").hub_id, "eu1");
    old_hub
        .expect_no_connection(Duration::from_millis(300))
        .await
        .expect("never dialed the old hub");
    assert_eq!(e.control.calls()[0].method, "GetAssignment");
    drop(conn);
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_or_refusing_station_api_falls_back_to_the_stored_hub() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    // Not active: logged, and the stored assignment is used.
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: Err(Status::failed_precondition("station is not active")),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        &hub.gateway_url(),
    )
    .await;
    let d = e.spawn();
    let conn = connected(&mut hub).await;
    drop(conn);
    stop(d).await;
    // Unreachable.
    drop(e.control);
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let dir = tempfile::tempdir().expect("tmp");
    let root = ca.write_root(dir.path());
    let path = write_enrolled_config(dir.path(), &format!("https://127.0.0.1:{port}"), &root);
    let config = Config::load_as(&path, Flavour::development()).expect("config");
    ca.write_enrolled(
        &StateDir::new(&config.state_dir),
        Uuid::new_v4(),
        fresh(),
        "sa1",
        &hub.gateway_url(),
    );
    let d = stationd::spawn(
        &config,
        Arc::new(FakeBackend::default()),
        common::fast_options(),
    )
    .expect("spawn");
    let conn = connected(&mut hub).await;
    drop(conn);
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_by_station_api_at_startup_stops() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: Err(Status::permission_denied("revoked")),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        &hub.gateway_url(),
    )
    .await;
    let d = e.spawn();
    exits_revoked(d).await;
    hub.expect_no_connection(Duration::from_millis(200))
        .await
        .expect("never connected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_assignment_goodbye_asks_station_api_first() {
    let ca = Arc::new(TestCa::new());
    let mut hub_a = ca.hub().await;
    let mut hub_b = ca.hub().await;
    let mut hub_c = ca.hub().await;
    let env = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: assign("sa1", &hub_a.gateway_url()),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        &hub_a.gateway_url(),
    )
    .await;
    let daemon = env.spawn();
    let mut conn = connected(&mut hub_a).await;
    // The home hub changed: SHUTDOWN with refresh_assignment.
    env.control.set(Script {
        assignment: assign("sa2", &hub_b.gateway_url()),
        renew: sign_days(30),
    });
    conn.send(v1::HubMessage {
        body: Some(v1::hub_message::Body::Goodbye(v1::Goodbye {
            reason: v1::goodbye::Reason::Shutdown as i32,
            message: "moved".into(),
            retry_after_ms: 0,
            refresh_assignment: true,
        })),
    })
    .await
    .expect("goodbye");
    conn.close();
    let mut conn = connected(&mut hub_b).await;
    assert_eq!(env.state().assignment().expect("stored").hub_id, "sa2");
    // REASSIGNED alone also refreshes.
    env.control.set(Script {
        assignment: assign("sa3", &hub_c.gateway_url()),
        renew: sign_days(30),
    });
    conn.goodbye(v1::goodbye::Reason::Reassigned, 0)
        .await
        .expect("goodbye");
    let conn = connected(&mut hub_c).await;
    let methods: Vec<&str> = env.control.calls().iter().map(|call| call.method).collect();
    assert_eq!(methods, ["GetAssignment"; 3]);
    drop(conn);
    stop(daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_certificate_is_renewed_and_the_tunnel_reconnects_with_it() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: assign("sa1", &hub.gateway_url()),
            renew: sign_days(30),
        },
        due(),
        "sa1",
        &hub.gateway_url(),
    )
    .await;
    e.control.close_gate();
    let d = e.spawn();
    let mut observed = d.observed();
    let mut state = d.state();
    let mut first = connected(&mut hub).await;
    wait_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    assert_eq!(observed.borrow().serial, e.serial);
    // Let the renewal through: the tunnel closes and comes back at once
    // with the new certificate.
    e.control.open_gate(1);
    let renewed = wait_observed(&mut observed, |o| o.renewal.last_renewal_unix_s.is_some()).await;
    first.expect_closed(WAIT).await.expect("old tunnel closed");
    let started = std::time::Instant::now();
    let second = connected(&mut hub).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reconnected without backoff"
    );
    assert_eq!(second.station_id(), Some(e.station_id));
    let o = wait_observed(&mut observed, |o| o.serial != e.serial).await;
    assert_eq!(o.serial, e.disk_serial(), "the new certificate is in use");
    assert!(renewed.renewal.last_error.is_none());
    assert!(
        o.renewal.next_renewal_unix_s.expect("next") > time_now().unix_timestamp() + 19 * 86_400
    );
    let renewals: Vec<Call> = e
        .control
        .calls()
        .into_iter()
        .filter(|c| c.method == "RenewCertificate")
        .collect();
    assert_eq!(renewals.len(), 1);
    assert_eq!(renewals[0].serial, e.serial);
    drop(second);
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_refused_as_revoked_stops() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: assign("sa1", &hub.gateway_url()),
            renew: Renew::Fail(Status::permission_denied("revoked")),
        },
        due(),
        "sa1",
        &hub.gateway_url(),
    )
    .await;
    e.control.close_gate();
    let d = e.spawn();
    let conn = connected(&mut hub).await;
    e.control.open_gate(1);
    exits_revoked(d).await;
    drop(conn);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_revocation_with_a_newer_certificate_on_disk_reconnects() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: assign("sa1", &hub.gateway_url()),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        &hub.gateway_url(),
    )
    .await;
    let d = e.spawn();
    let mut observed = d.observed();
    let mut conn = connected(&mut hub).await;
    // A new certificate lands on disk (a renewal or `enroll --force`),
    // then the hub refuses the old one.
    let (key, cert, serial) = ca.station_credentials(e.station_id, fresh().0, fresh().1);
    e.state()
        .swap_credentials(&NewCredentials {
            key_pem: &key,
            cert_pem: &cert,
            chain_pem: &ca.station.pem,
            gateway_ca_pem: None,
        })
        .expect("swap");
    conn.goodbye(v1::goodbye::Reason::Revoked, 0)
        .await
        .expect("goodbye");
    let mut conn = connected(&mut hub).await;
    let o = wait_observed(&mut observed, |o| o.serial == serial).await;
    assert_eq!(o.serial, serial);
    assert!(!d.is_finished());
    // Now the new one is refused too: nothing newer, so it stops.
    conn.goodbye(v1::goodbye::Reason::Revoked, 0)
        .await
        .expect("goodbye");
    exits_revoked(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_skew_reaches_status_and_doctor() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let e = Env::with_ca(
        Arc::clone(&ca),
        Script {
            assignment: assign("sa1", &hub.gateway_url()),
            renew: sign_days(30),
        },
        fresh(),
        "sa1",
        &hub.gateway_url(),
    )
    .await;
    let d = e.spawn();
    let status_file = tokio::spawn(stationd::local_status::keep_written(
        e.config.status_path(),
        d.station_id().to_string(),
        d.state(),
        d.observed(),
        d.last_error(),
        Arc::new(FakeBackend::default()),
    ));
    let mut observed = d.observed();
    let mut conn = hub.accept(WAIT).await.expect("connects");
    let hello = conn.expect_hello(WAIT).await.expect("hello");
    // The hub's clock is 45 s ahead of this one.
    let hub_time = time_now().unix_timestamp() * 1000 + 45_000;
    conn.send_welcome(v1::Welcome {
        protocol_version: hello.protocol_version,
        connection_id: "skewed".into(),
        status_interval_ms: 60_000,
        max_message_bytes: 1 << 20,
        hub_time_unix_ms: hub_time,
    })
    .await
    .expect("welcome");
    let o = wait_observed(&mut observed, |o| o.clock_skew_ms.is_some()).await;
    let skew = o.clock_skew_ms.expect("skew");
    assert!((43_000..=47_000).contains(&skew), "{skew}");
    // status.json, `stationd status` and `stationd doctor`.
    let mut text = String::new();
    for _ in 0..100 {
        text = stationd::local_status::report(&e.config, time_now().unix_timestamp()).render();
        if text.contains("clock skew") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(text.contains("s behind the hub's (WARNING"), "{text}");
    assert!(text.contains("renewal due"), "{text}");
    assert!(text.contains("next renewal"), "{text}");
    let checks = stationd::doctor::run_config(
        &e.config,
        std::path::Path::new("stationd.toml"),
        &stationd::doctor::Environment::default(),
    )
    .await;
    let skew_check = checks
        .iter()
        .find(|c| c.name == "clock-skew")
        .expect("clock-skew check");
    assert_eq!(
        skew_check.level,
        stationd::doctor::Level::Warn,
        "{skew_check}"
    );
    let api = checks
        .iter()
        .find(|c| c.name == "station-api")
        .expect("station-api check");
    assert_eq!(api.level, stationd::doctor::Level::Pass, "{api}");
    drop(conn);
    stop(d).await;
    tokio::time::timeout(WAIT, status_file)
        .await
        .expect("writer ends")
        .expect("join");
}
