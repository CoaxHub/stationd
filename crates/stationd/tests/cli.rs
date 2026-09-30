// SPDX-License-Identifier: GPL-3.0-only
//! The `stationd` binary: version, check-config, and run + SIGTERM.

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use station_conformance::LocalPolicy;
use station_conformance::scenarios::{Harness, ScenarioConfig};

const BIN: &str = env!("CARGO_BIN_EXE_stationd");

#[test]
fn version() {
    let out = Command::new(BIN).arg("--version").output().expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text.trim(),
        format!("stationd {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_errors_exit_2() {
    let out = Command::new(BIN).output().expect("run");
    assert_eq!(out.status.code(), Some(2));
    let out = Command::new(BIN)
        .args(["check-config", "--config", "/nonexistent.toml"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read config file"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_config_then_run_until_sigterm() {
    let mut h = Harness::new(LocalPolicy::default()).await.expect("harness");
    let path = common::write_config(&h.target);
    let out = Command::new(BIN)
        .args(["check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(&h.target.station_id.to_string()));

    let child = Command::new(BIN)
        .args(["run", "--config"])
        .arg(&path)
        .env("STATIOND_LOG_FORMAT", "json")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut conn = h
        .connect(Duration::from_secs(10), &ScenarioConfig::fast())
        .await
        .expect("the binary connects");
    // Make sure the session is established before the signal.
    let ping = station_proto::v1::request::Command::Ping(station_proto::v1::Ping {
        nonce: 1,
        hub_time_unix_ms: 0,
    });
    conn.call(ping, Duration::from_secs(5)).await.expect("pong");
    let pid = child.id().to_string();
    let killed = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("kill");
    assert!(killed.success());
    let sent = conn
        .expect_closed(Duration::from_secs(10))
        .await
        .expect("stream closed");
    drop(conn);
    assert!(sent.iter().any(|m| matches!(
        &m.body,
        Some(station_proto::v1::station_message::Body::Event(e))
            if e.kind == station_proto::v1::event::Kind::ShuttingDown as i32
    )));
    let out = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .expect("join")
        .expect("wait");
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{log}");
    assert!(log.contains("\"connected to the hub\""), "JSON logs: {log}");
    assert!(!log.contains("PRIVATE KEY"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_exits_3() {
    let mut h = Harness::new(LocalPolicy::default()).await.expect("harness");
    let path = common::write_config(&h.target);
    let child = Command::new(BIN)
        .args(["run", "-c"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut conn = h
        .connect(Duration::from_secs(10), &ScenarioConfig::fast())
        .await
        .expect("connects");
    conn.goodbye(station_proto::v1::goodbye::Reason::Revoked, 0)
        .await
        .expect("goodbye");
    let out = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .expect("join")
        .expect("wait");
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("revoked"));
}
