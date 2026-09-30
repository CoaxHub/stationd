// SPDX-License-Identifier: GPL-3.0-only
//! stationd behaviour beyond the conformance suite: connection state,
//! Goodbye reasons and backoff delays, Welcome edge cases, backend events,
//! TLS failure details, and the library/CLI entry points.

// Static [credentials] and [backend] are development-only settings.
#![cfg(not(feature = "packaged"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use station_backend::FakeBackend;
use station_conformance::scenarios::{Harness, ScenarioConfig};
use station_conformance::{HubTls, LocalPolicy, StationConn, TestPki, TlsVersions};
use station_proto::v1;
use stationd::daemon::{ConnectionState, StopReason};
use stationd::tls::Connector;
use stationd::{DaemonHandle, RunError, RunOptions};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use v1::hub_message::Body as HubBody;
use v1::request::Command;
use v1::station_message::Body;

const WAIT: Duration = Duration::from_secs(5);

fn cfg() -> ScenarioConfig {
    ScenarioConfig::fast()
}

async fn harness() -> Harness {
    Harness::new(LocalPolicy::default()).await.expect("harness")
}

fn spawn_with(h: &Harness, backend: FakeBackend, options: RunOptions) -> DaemonHandle {
    let config = common::load(&common::write_config(&h.target));
    stationd::spawn(&config, Arc::new(backend), options).expect("spawn")
}

fn spawn(h: &Harness) -> DaemonHandle {
    spawn_with(h, FakeBackend::default(), common::fast_options())
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

async fn stop(mut d: DaemonHandle) {
    d.shutdown();
    tokio::time::timeout(WAIT, d.join())
        .await
        .expect("stops")
        .expect("clean");
}

async fn connected(h: &mut Harness) -> StationConn {
    h.connect(WAIT, &cfg()).await.expect("connected")
}

async fn goodbye_gives_backoff(reason: v1::goodbye::Reason, expected: Duration) {
    let mut h = harness().await;
    let d = spawn(&h);
    let mut state = d.state();
    let mut conn = connected(&mut h).await;
    let s = wait_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    assert_eq!(
        s,
        ConnectionState::Connected {
            connection_id: format!("conformance-{}", conn.id()),
            version: station_proto::PROTOCOL_VERSION,
        }
    );
    conn.goodbye(reason, 0).await.expect("goodbye");
    let s = wait_state(&mut state, |s| matches!(s, ConnectionState::Backoff { .. })).await;
    assert_eq!(
        s,
        ConnectionState::Backoff { delay: expected },
        "{reason:?}"
    );
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_waits_the_disabled_interval() {
    goodbye_gives_backoff(v1::goodbye::Reason::Disabled, Duration::from_millis(700)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_version_waits_the_long_interval() {
    goodbye_gives_backoff(
        v1::goodbye::Reason::UnsupportedVersion,
        Duration::from_millis(900),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_goodbyes_reconnect_with_backoff() {
    for reason in [
        v1::goodbye::Reason::Reassigned,
        v1::goodbye::Reason::ProtocolViolation,
        v1::goodbye::Reason::DuplicateConnection,
    ] {
        let mut h = harness().await;
        let d = spawn(&h);
        let mut conn = connected(&mut h).await;
        conn.goodbye(reason, 0).await.expect("goodbye");
        let mut next = connected(&mut h).await;
        let r = next
            .call(
                Command::Ping(v1::Ping {
                    nonce: 1,
                    hub_time_unix_ms: 0,
                }),
                WAIT,
            )
            .await
            .expect("ping");
        assert!(matches!(r.payload, Some(v1::response::Payload::Pong(_))));
        drop(next);
        stop(d).await;
    }
    // A reason from a newer hub is treated like any disconnect.
    let mut h = harness().await;
    let d = spawn(&h);
    let mut conn = connected(&mut h).await;
    conn.send(v1::HubMessage {
        body: Some(HubBody::Goodbye(v1::Goodbye {
            reason: 77,
            message: "line\nbreak".into(),
            ..Default::default()
        })),
    })
    .await
    .expect("send");
    conn.close();
    let _next = connected(&mut h).await;
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_welcome_is_an_unsupported_version() {
    let mut h = harness().await;
    let d = spawn(&h);
    let mut state = d.state();
    let mut conn = h.hub.accept(WAIT).await.expect("accept");
    conn.expect_hello(WAIT).await.expect("hello");
    conn.send_welcome(v1::Welcome {
        protocol_version: Some(v1::ProtocolVersion { major: 2, minor: 0 }),
        connection_id: "x".into(),
        ..Default::default()
    })
    .await
    .expect("welcome");
    let s = wait_state(&mut state, |s| matches!(s, ConnectionState::Backoff { .. })).await;
    assert_eq!(
        s,
        ConnectionState::Backoff {
            delay: Duration::from_millis(900)
        }
    );
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goodbye_instead_of_welcome() {
    let mut h = harness().await;
    let d = spawn(&h);
    let mut state = d.state();
    let mut conn = h.hub.accept(WAIT).await.expect("accept");
    conn.expect_hello(WAIT).await.expect("hello");
    conn.goodbye(v1::goodbye::Reason::Disabled, 0)
        .await
        .expect("goodbye");
    let s = wait_state(&mut state, |s| matches!(s, ConnectionState::Backoff { .. })).await;
    assert_eq!(
        s,
        ConnectionState::Backoff {
            delay: Duration::from_millis(700)
        }
    );
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_welcome_or_a_request_first_means_reconnect() {
    let mut h = harness().await;
    let options = RunOptions {
        welcome_timeout: Duration::from_millis(300),
        ..common::fast_options()
    };
    let d = spawn_with(&h, FakeBackend::default(), options);
    // 1: never welcomed.
    let mut silent = h.hub.accept(WAIT).await.expect("accept");
    silent.expect_hello(WAIT).await.expect("hello");
    // The abandoned attempt really ends its stream (no leaked connection).
    silent
        .expect_closed(WAIT)
        .await
        .expect("station closes the stream");
    // 2: a request before Welcome.
    let mut rude = h
        .hub
        .accept(WAIT)
        .await
        .expect("reconnect after welcome timeout");
    rude.expect_hello(WAIT).await.expect("hello");
    rude.send(v1::HubMessage {
        body: Some(HubBody::Request(v1::Request {
            request_id: 1,
            command: Some(Command::Ping(v1::Ping::default())),
        })),
    })
    .await
    .expect("send");
    // 3: the hub closes before Welcome.
    let mut closer = h
        .hub
        .accept(WAIT)
        .await
        .expect("reconnect after bad first message");
    closer.expect_hello(WAIT).await.expect("hello");
    closer.close();
    let _ok = connected(&mut h).await;
    drop((silent, rude, closer));
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn odd_hub_messages_are_ignored() {
    let mut h = harness().await;
    let d = spawn(&h);
    let mut conn = connected(&mut h).await;
    conn.send(v1::HubMessage { body: None })
        .await
        .expect("send");
    conn.send(v1::HubMessage {
        body: Some(HubBody::Welcome(v1::Welcome::default())),
    })
    .await
    .expect("send");
    let r = conn
        .call(
            Command::Ping(v1::Ping {
                nonce: 5,
                hub_time_unix_ms: 0,
            }),
            WAIT,
        )
        .await
        .expect("still serving");
    assert!(matches!(r.payload, Some(v1::response::Payload::Pong(p)) if p.nonce == 5));
    stop(d).await;
}

fn events(msgs: &[(tokio::time::Instant, v1::StationMessage)]) -> Vec<v1::Event> {
    msgs.iter()
        .filter_map(|(_, m)| match &m.body {
            Some(Body::Event(e)) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_events_are_forwarded_and_update_tables() {
    let mut h = harness().await;
    let backend = FakeBackend::default();
    let d = spawn_with(&h, backend.clone(), common::fast_options());
    let mut conn = connected(&mut h).await;
    let open = |id: u32| {
        Command::OpenChannel(v1::OpenChannel {
            channel_id: id,
            frequency_hz: 6_200_000,
            mode: v1::Mode::Am as i32,
            low_cut_hz: -4000,
            high_cut_hz: 4000,
            audio: Some(v1::AudioParams {
                codec: v1::AudioCodec::Opus as i32,
                bitrate_bps: 16_000,
                sample_rate_hz: 12_000,
                frame_duration_ms: 20,
            }),
            squelch_snr_db: None,
        })
    };
    let r = conn.call(open(1), WAIT).await.expect("open");
    assert_eq!(r.result.map(|r| r.code), Some(v1::ResultCode::Ok as i32));
    // Retune away: the backend closes channel 1 by itself.
    let retune = Command::SetFrontend(v1::SetFrontend {
        center_frequency_hz: Some(14_100_000),
        ..Default::default()
    });
    let r = conn.call(retune, WAIT).await.expect("retune");
    assert_eq!(r.result.map(|r| r.code), Some(v1::ResultCode::Ok as i32));
    backend.emit_event(v1::Event {
        kind: v1::event::Kind::DeviceLost as i32,
        detail: "x".repeat(1000),
        ..Default::default()
    });
    let got = conn
        .collect(Duration::from_millis(500))
        .await
        .expect("collect");
    let kinds: Vec<i32> = events(&got).iter().map(|e| e.kind).collect();
    assert!(
        kinds.contains(&(v1::event::Kind::ChannelClosed as i32)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(v1::event::Kind::FrontendChanged as i32)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(v1::event::Kind::DeviceLost as i32)),
        "{kinds:?}"
    );
    // The table forgot channel 1: the latest status says 0 channels, and a
    // close gets NOT_FOUND.
    let last_status = got
        .iter()
        .rev()
        .find_map(|(_, m)| match &m.body {
            Some(Body::Status(s)) => Some(s.clone()),
            _ => None,
        })
        .expect("a status");
    assert_eq!(last_status.active_channels, 0);
    assert!(last_status.host.is_some() && last_status.backend.is_some());
    let r = conn
        .call(
            Command::CloseChannel(v1::CloseChannel { channel_id: 1 }),
            WAIT,
        )
        .await
        .expect("close");
    assert_eq!(
        r.result.map(|r| r.code),
        Some(v1::ResultCode::NotFound as i32)
    );
    // Subscription closed by the backend, and a backend restart.
    let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
        subscription_id: 2,
        range: Some(v1::FrequencyRange {
            start_hz: 14_000_000,
            end_hz: 14_200_000,
        }),
        bins: 128,
        row_interval_ms: 100,
        quantization: None,
        compression: v1::WaterfallCompression::Zstd as i32,
    });
    let r = conn.call(sub, WAIT).await.expect("subscribe");
    assert_eq!(r.result.map(|r| r.code), Some(v1::ResultCode::Ok as i32));
    backend.emit_event(v1::Event {
        kind: v1::event::Kind::SubscriptionClosed as i32,
        subscription_id: Some(2),
        ..Default::default()
    });
    backend.emit_event(v1::Event {
        kind: v1::event::Kind::BackendRestarted as i32,
        ..Default::default()
    });
    let got = conn
        .collect(Duration::from_millis(300))
        .await
        .expect("collect");
    assert_eq!(events(&got).len(), 2);
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_while_waiting_to_reconnect() {
    let mut h = harness().await;
    h.hub.stop().await;
    let d = spawn(&h);
    let mut state = d.state();
    let s = wait_state(&mut state, |s| matches!(s, ConnectionState::Backoff { .. })).await;
    assert!(matches!(s, ConnectionState::Backoff { delay } if delay <= Duration::from_millis(300)));
    assert_eq!(d.boot_id(), d.boot_id());
    assert!(!d.is_finished());
    let mut d = d;
    d.shutdown();
    d.join().await.expect("clean stop");
    assert!(d.is_finished());
    assert_eq!(
        d.current_state(),
        ConnectionState::Stopped(StopReason::Shutdown)
    );
    assert!(matches!(d.join().await, Err(RunError::AlreadyJoined)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_before_welcome() {
    let mut h = harness().await;
    let d = spawn(&h);
    let mut conn = h.hub.accept(WAIT).await.expect("accept");
    conn.expect_hello(WAIT).await.expect("hello");
    stop(d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_api_with_external_shutdown_and_revocation() {
    let mut h = harness().await;
    let config = common::load(&common::write_config(&h.target));
    let shutdown = CancellationToken::new();
    let (tx, mut rx) = watch::channel(ConnectionState::Connecting { attempt: 0 });
    let task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            stationd::run(
                &config,
                Arc::new(FakeBackend::default()),
                common::fast_options(),
                shutdown,
                tx,
            )
            .await
        }
    });
    let mut conn = connected(&mut h).await;
    wait_state(&mut rx, |s| matches!(s, ConnectionState::Connected { .. })).await;
    conn.goodbye(v1::goodbye::Reason::Revoked, 0)
        .await
        .expect("goodbye");
    let result = tokio::time::timeout(WAIT, task)
        .await
        .expect("ends")
        .expect("join");
    assert!(matches!(result, Err(RunError::Revoked)));
    assert_eq!(*rx.borrow(), ConnectionState::Stopped(StopReason::Revoked));
    drop(shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_fails_on_bad_credentials() {
    let h = harness().await;
    let mut target = h.target.clone();
    target.key = target.cert.clone();
    let config = common::load(&common::write_config(&target));
    let e = stationd::spawn(
        &config,
        Arc::new(FakeBackend::default()),
        RunOptions::default(),
    )
    .expect_err("key is a certificate");
    assert!(matches!(e, RunError::Tls(_)), "{e}");
    assert!(format!("{:?}", RunOptions::default()).contains("backoff"));
}

async fn connector_error(hub_tls: impl FnOnce(&TestPki) -> HubTls) -> String {
    let h = Harness::with_hub_tls(LocalPolicy::default(), hub_tls)
        .await
        .expect("harness");
    let config = common::load(&common::write_config(&h.target));
    let creds = stationd::credentials::load(&config).expect("credentials");
    let connector = Connector::new(creds.tls.config, &creds.gateway).expect("connector");
    connector
        .connect()
        .await
        .expect_err("must fail")
        .to_string()
}

#[tokio::test]
async fn gateway_from_another_ca_fails_certificate_verification() {
    let other = TestPki::generate().expect("pki");
    let e = connector_error(|pki| HubTls {
        cert_chain_pem: other.hub_cert_pem.clone(),
        key_pem: other.hub_key_pem.clone(),
        client_ca_pem: pki.ca_pem.clone(),
        versions: TlsVersions::Tls13Only,
    })
    .await;
    assert!(
        e.contains("UnknownIssuer") || e.contains("invalid peer certificate"),
        "{e}"
    );
}

#[tokio::test]
async fn tls12_gateway_is_refused() {
    let e = connector_error(|pki| HubTls {
        versions: TlsVersions::Tls12Only,
        ..pki.hub_tls()
    })
    .await;
    assert!(e.contains("TLS handshake"), "{e}");
}

#[tokio::test]
async fn gateway_certificate_for_another_name_is_refused() {
    let e = connector_error(|pki| {
        let (cert, key) = pki
            .issue_hub(&["gw.elsewhere.example"], &[])
            .expect("hub cert");
        HubTls {
            cert_chain_pem: cert,
            key_pem: key,
            ..pki.hub_tls()
        }
    })
    .await;
    assert!(
        e.contains("NotValidForName") || e.contains("not valid for name"),
        "{e}"
    );
}

#[tokio::test]
async fn nothing_listening_is_a_tcp_error() {
    let mut h = harness().await;
    h.hub.stop().await;
    let config = common::load(&common::write_config(&h.target));
    let creds = stationd::credentials::load(&config).expect("credentials");
    let connector = Connector::new(creds.tls.config, &creds.gateway).expect("connector");
    let e = connector.connect().await.expect_err("refused").to_string();
    assert!(e.contains("TCP connect"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_check_config_and_run() {
    let mut h = harness().await;
    let path = common::write_config(&h.target);
    let summary = stationd::cli::check_config(&path).expect("valid");
    assert!(
        summary.contains(&h.target.station_id.to_string()),
        "{summary}"
    );
    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let stop = stop.clone();
        let path = path.clone();
        async move { stationd::cli::run(&path, stop.cancelled_owned()).await }
    });
    let conn = connected(&mut h).await;
    stop.cancel();
    drop(conn);
    tokio::time::timeout(WAIT, run)
        .await
        .expect("stops")
        .expect("join")
        .expect("clean exit");
    // Revocation ends `run` by itself with the revoked exit code.
    let run = tokio::spawn({
        let path = path.clone();
        async move { stationd::cli::run(&path, std::future::pending()).await }
    });
    let mut conn = connected(&mut h).await;
    conn.goodbye(v1::goodbye::Reason::Revoked, 0)
        .await
        .expect("goodbye");
    let e = tokio::time::timeout(WAIT, run)
        .await
        .expect("ends")
        .expect("join")
        .expect_err("revoked");
    assert_eq!(e.exit_code(), stationd::cli::EXIT_REVOKED);
}
