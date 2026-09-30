// SPDX-License-Identifier: GPL-3.0-only
//! The `stationd` binary: version, check-config, and run + SIGTERM.

// Static [credentials] and [backend] are development-only settings.
#![cfg(not(feature = "packaged"))]

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
    // `stationd status` shows it.
    let status =
        std::fs::read_to_string(h.target.work_dir.join("state/status.json")).expect("status.json");
    assert!(status.contains("\"revoked\""), "{status}");
    let out = stationd(&["status"], &path);
    assert!(
        text(&out).contains("certificate was revoked"),
        "{}",
        text(&out)
    );
}

mod enroll_support;

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use enroll_support::{Behaviour, EnrollServer, TOKEN, TestCa};

fn stationd(args: &[&str], config: &Path) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .arg("--config")
        .arg(config)
        .env("RUST_LOG", "trace")
        .output()
        .expect("run")
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

async fn wait_for_status(path: &Path, want: &str) -> String {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if s.contains(want) {
                return s;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("status.json never contained {want}");
}

/// The owner's path through the binary: enroll (token on stdin), status,
/// run (connects to the assigned hub, keeps status.json), doctor, unenroll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::too_many_lines,
    reason = "the owner's path reads top to bottom"
)]
async fn enroll_status_run_doctor_unenroll() {
    let ca = Arc::new(TestCa::new());
    let mut hub = ca.hub().await;
    let server = EnrollServer::start(
        Arc::clone(&ca),
        &ca.server_cert(&["localhost"]),
        Behaviour::Sign {
            gateway_url: hub.gateway_url(),
        },
    )
    .await;
    let dir = tempfile::tempdir().expect("tmp");
    let root = ca.write_root(dir.path());
    let config = enroll_support::write_config(dir.path(), &server.url(), &root);
    let status_file = dir.path().join("state/status.json");

    // Not enrolled yet.
    let out = stationd(&["status"], &config);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("not enrolled"), "{}", text(&out));
    let out = stationd(&["run"], &config);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("stationd enroll"), "{}", text(&out));
    let out = stationd(&["check-config"], &config);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("not enrolled yet"), "{}", text(&out));

    // Enroll with the token on standard input, logging at TRACE.
    let mut child = Command::new(BIN)
        .args(["enroll", "--config"])
        .arg(&config)
        .env("RUST_LOG", "trace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{TOKEN}\n").as_bytes())
        .expect("token");
    let out = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .expect("join")
        .expect("wait");
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(all.contains(&server.station_id.to_string()), "{all}");
    assert!(all.contains("TRACE"), "trace logging was on");
    assert!(!all.contains(&TOKEN[8..]), "the token was echoed or logged");
    assert_eq!(server.seen()[0].token, TOKEN);
    // Again: refused without --force, before asking for a token.
    let out = stationd(&["enroll"], &config);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("already enrolled"), "{}", text(&out));

    let out = stationd(&["status", "--json"], &config);
    assert!(out.status.success(), "{}", text(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(report["enrolled"], true);
    assert_eq!(report["hub_id"], "sa1");
    assert_eq!(report["station_id"], server.station_id.to_string());
    assert_eq!(report["daemon_running"], false);

    // Run: connects to the assigned hub and reports the tunnel state.
    let run = Command::new(BIN)
        .args(["run", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut conn = hub.accept(Duration::from_secs(10)).await.expect("connects");
    conn.expect_hello(Duration::from_secs(5))
        .await
        .expect("hello");
    conn.welcome(60_000).await.expect("welcome");
    let status = wait_for_status(&status_file, "\"connected\"").await;
    assert!(status.contains(&server.station_id.to_string()), "{status}");
    let out = stationd(&["status"], &config);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("running (pid"), "{t}");
    assert!(t.contains("connected (connection conformance-"), "{t}");
    assert!(t.contains("home hub     sa1"), "{t}");

    // Doctor: every applicable check passes.
    let out = stationd(&["doctor"], &config);
    let t = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{t}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in [
        "pass  config",
        "pass  enrollment",
        "skip  sdr",
        "skip  radiod",
        "pass  enroll",
        "pass  gateway",
        "pass  clock",
    ] {
        assert!(t.contains(line), "{line}: {t}");
    }

    // SIGTERM: a clean stop, recorded in status.json.
    let killed = Command::new("kill")
        .args(["-TERM", &run.id().to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let out = tokio::task::spawn_blocking(move || run.wait_with_output())
        .await
        .expect("join")
        .expect("wait");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(conn);
    wait_for_status(&status_file, "\"stopped\"").await;

    // Unenroll: credentials gone, the daemon refuses to run.
    let out = stationd(&["unenroll"], &config);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("removed key.pem"), "{}", text(&out));
    let out = stationd(&["unenroll"], &config);
    assert!(text(&out).contains("nothing") || text(&out).contains("not enrolled"));
    let out = stationd(&["run"], &config);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let out = stationd(&["doctor"], &config);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("FAIL  enrollment"), "{}", text(&out));
}

#[test]
fn enroll_errors() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = dir.path().join("stationd.toml");
    std::fs::write(&config, "[sdr]\nprofile = \"siggen\"\n").expect("write");
    // No [endpoints] in a development build.
    let out = stationd(&["enroll", "--token", TOKEN], &config);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("[endpoints]"), "{}", text(&out));
    // A malformed token is refused without being echoed.
    let root = dir.path().join("root.pem");
    std::fs::write(&root, "x").expect("write");
    enroll_support::write_config(dir.path(), "https://localhost:1", &root);
    let out = stationd(&["enroll", "--token", "chs_enr_SECRETSECRET"], &config);
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(
        text(&out).contains("not an enrollment token"),
        "{}",
        text(&out)
    );
    assert!(!text(&out).contains("SECRETSECRET"), "{}", text(&out));
    // Logs to a pipe (the journal, the installer) carry no colour codes.
    assert!(!text(&out).contains('\u{1b}'), "{:?}", text(&out));
    // An empty line on stdin.
    let out = Command::new(BIN)
        .args(["enroll", "--config"])
        .arg(&config)
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(text(&out).contains("no enrollment token"), "{}", text(&out));
}

#[test]
fn backend_config_renders_the_profile() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = dir.path().join("stationd.toml");
    std::fs::write(&config, "[sdr]\nprofile = \"siggen\"\n").expect("write");
    let out = stationd(&["backend-config", "--out", "-"], &config);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hardware = sig_gen"), "{stdout}");
    assert!(stdout.contains("status = 239.77.0.6\n"), "{stdout}");

    std::fs::write(
        &config,
        "[sdr]\nprofile = \"rtlsdr\"\ncenter_frequency_hz = 145500000\n",
    )
    .expect("write");
    let rendered = dir.path().join("radiod.conf");
    let out = Command::new(BIN)
        .args(["backend-config", "--config"])
        .arg(&config)
        .arg("--out")
        .arg(&rendered)
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", text(&out));
    let conf = std::fs::read_to_string(&rendered).expect("rendered");
    assert!(conf.contains("frequency = 145500000\n"), "{conf}");
    let out = Command::new(BIN)
        .args(["backend-config", "--config"])
        .arg(&config)
        .args(["--out", "-", "--templates", "/nonexistent"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("/nonexistent/rtlsdr.conf"),
        "{}",
        text(&out)
    );

    // Unknown profile: a config error listing the supported ones.
    std::fs::write(&config, "[sdr]\nprofile = \"airspy\"\n").expect("write");
    let out = stationd(&["backend-config", "--out", "-"], &config);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("supported: rtlsdr, rtlsdr-hf, siggen"),
        "{}",
        text(&out)
    );

    // No [sdr]: nothing to render.
    std::fs::write(&config, "[backend]\nkind = \"fake\"\n").expect("write");
    let out = stationd(&["backend-config", "--out", "-"], &config);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("no [sdr] profile"), "{}", text(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_credentials_cannot_enroll_or_unenroll() {
    let h = Harness::new(LocalPolicy::default()).await.expect("harness");
    let path = common::write_config(&h.target);
    let out = stationd(&["enroll", "--token", TOKEN], &path);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        text(&out).contains("static [credentials]"),
        "{}",
        text(&out)
    );
    let out = stationd(&["unenroll"], &path);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = stationd(&["status"], &path);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("static development credentials"),
        "{}",
        text(&out)
    );
}

/// The token prompt on a terminal: echo is off before the prompt appears,
/// the token is never echoed, and the terminal is restored afterwards
/// (the installer hands stationd the terminal).
#[test]
fn token_prompt_hides_the_input() {
    use rustix::termios::{LocalModes, tcgetattr};
    use std::io::{Read as _, Write as _};

    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path().join("root.pem");
    std::fs::write(&root, "x").expect("write");
    let config = enroll_support::write_config(dir.path(), "https://localhost:1", &root);

    let master =
        rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
            .expect("openpt");
    rustix::pty::grantpt(&master).expect("grantpt");
    rustix::pty::unlockpt(&master).expect("unlockpt");
    let name = rustix::pty::ptsname(&master, Vec::new()).expect("ptsname");
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().expect("utf-8 name"))
        .expect("open the terminal");
    assert!(
        tcgetattr(&slave)
            .expect("termios")
            .local_modes
            .contains(LocalModes::ECHO)
    );

    let child = Command::new(BIN)
        .args(["enroll", "--config"])
        .arg(&config)
        .stdin(slave.try_clone().expect("clone"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    // Wait for stationd to turn echo off, then "type" a malformed token.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while tcgetattr(&slave)
        .expect("termios")
        .local_modes
        .contains(LocalModes::ECHO)
    {
        assert!(std::time::Instant::now() < deadline, "echo never went off");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut typing = std::fs::File::from(master.try_clone().expect("clone master"));
    typing.write_all(b"chs_enr_SECRETSECRET\n").expect("type");
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(
        text(&out).contains("Enrollment token (input hidden)"),
        "{}",
        text(&out)
    );
    assert!(!text(&out).contains("SECRETSECRET"), "{}", text(&out));
    // Echo is back on, and nothing was echoed to the terminal.
    assert!(
        tcgetattr(&slave)
            .expect("termios")
            .local_modes
            .contains(LocalModes::ECHO)
    );
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK).expect("nonblocking");
    let mut screen = Vec::new();
    let _ = std::fs::File::from(master).read_to_end(&mut screen);
    let screen = String::from_utf8_lossy(&screen);
    assert!(!screen.contains("SECRET"), "echoed: {screen:?}");
}
