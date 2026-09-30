// SPDX-License-Identifier: GPL-3.0-only
//! stationd with the ka9q backend (`Ka9qBackend`) against the fake radiod
//! of `backend-ka9q/tests/support`, in real time over loopback:
//!
//! - every station-conformance scenario, with the fake hub validating each
//!   message with the §5 rules;
//! - a scripted session that checks the media in detail (audio sequence
//!   across a retune, waterfall rows, S-meter readings);
//! - `stationd check-config` on a `[backend] kind = "ka9q"` config.

// Static [credentials] and [backend] are development-only settings.
#![cfg(not(feature = "packaged"))]
#![allow(
    clippy::too_many_lines,
    reason = "end-to-end scripts read top to bottom"
)]

mod common;
#[path = "../../backend-ka9q/tests/support/mod.rs"]
mod support;

use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use backend_ka9q::backend::{Ka9qBackend, Ka9qConfig};
use station_conformance::scenarios::{self, After, Harness, ScenarioConfig, run_scenario};
use station_conformance::{LocalPolicy, StationUnderTest as _};
use station_proto::v1;
use station_proto::v1::request::Command;
use station_proto::v1::response::Payload;
use station_proto::v1::station_message::Body;
use station_proto::validate::WaterfallDecoder;
use support::FakeRadiod;

/// A fake radiod and a ka9q backend connected to it.
async fn radiod() -> (FakeRadiod, Arc<Ka9qBackend>) {
    let fake = FakeRadiod::start().await;
    let config = Ka9qConfig::new(fake.addr, Some("127.0.0.1:0".parse().expect("addr")));
    let backend = Ka9qBackend::new(config).expect("backend");
    let client = backend.client().await.expect("client");
    fake.set_rtp_dest(client.local_data_addr());
    fake.set_status_dest(client.local_channel_status_addr().expect("status socket"));
    (fake, Arc::new(backend))
}

async fn run(name: &str) {
    let scenario = scenarios::by_name(name).expect("known scenario");
    let (fake, backend) = radiod().await;
    let mut station = common::InProcess::with_backend(backend);
    let cfg = ScenarioConfig::fast();
    if let Err(e) = (scenario.run)(&mut station, &cfg).await {
        panic!("{name} with the ka9q backend: {e}");
    }
    fake.assert_client_contract();
}

macro_rules! conformance {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $name() {
                run(stringify!($name)).await;
            }
        )*

        #[test]
        fn every_scenario_is_run() {
            let here = [$(stringify!($name)),*];
            let all: Vec<&str> = scenarios::ALL.iter().map(|s| s.name).collect();
            assert_eq!(here.as_slice(), all.as_slice());
        }
    };
}

conformance!(
    hello_then_welcome,
    answers_get_capabilities,
    answers_ping,
    answers_every_request_once,
    rejects_unknown_command_as_unsupported,
    admin_op_denied_by_default,
    admin_op_allowed_when_configured,
    set_frontend_denied_when_disabled,
    status_at_interval,
    reconnects_after_hub_restart,
    reconnects_after_connection_drop,
    honours_goodbye_retry_after,
    stops_on_revoked,
    sends_shutting_down_on_stop,
    rejects_untrusted_gateway,
    rejects_tls12_gateway,
    detects_dead_connection,
    opens_channel_and_streams_audio,
    subscribes_waterfall_and_streams_rows,
    drops_channels_on_disconnect,
);

fn ok(r: &v1::Response) -> bool {
    r.result
        .as_ref()
        .is_some_and(|res| res.code == v1::ResultCode::Ok as i32)
}

/// A full session through the tunnel: capabilities, a channel with a retune,
/// a waterfall, backend health in `StationStatus`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_through_the_tunnel() {
    let (fake, backend) = radiod().await;
    let mut station = common::InProcess::with_backend(backend);
    let cfg = ScenarioConfig::fast();
    let harness = Harness::new(LocalPolicy::default()).await;
    let result = run_scenario(&mut station, &cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, &cfg).await?;
        let within = cfg.response_within;
        let r = conn
            .call(Command::GetCapabilities(v1::GetCapabilities {}), within)
            .await?;
        let Some(Payload::Capabilities(caps)) = r.payload else {
            panic!("no capabilities: {r:?}");
        };
        assert_eq!(caps.sample_rate_hz, 24_000_000);
        assert_eq!(caps.max_channels, 16, "local policy caps the backend's 64");

        let open = Command::OpenChannel(v1::OpenChannel {
            channel_id: 3,
            frequency_hz: 10_000_000,
            mode: v1::Mode::Am as i32,
            low_cut_hz: -5000,
            high_cut_hz: 5000,
            audio: Some(v1::AudioParams {
                codec: v1::AudioCodec::Opus as i32,
                bitrate_bps: 16_000,
                sample_rate_hz: 12_000,
                frame_duration_ms: 20,
            }),
            squelch_snr_db: None,
        });
        let r = conn.call(open, within).await?;
        assert!(ok(&r), "{r:?}");
        let got = conn.collect(Duration::from_millis(1000)).await?;
        let retune = Command::UpdateChannel(v1::UpdateChannel {
            channel_id: 3,
            frequency_hz: Some(9_990_000),
            ..Default::default()
        });
        let r = conn.call(retune, within).await?;
        let Some(Payload::ChannelUpdated(u)) = r.payload else {
            panic!("no ChannelUpdated: {r:?}");
        };
        assert_eq!(u.frequency_hz, 9_990_000);
        let after = conn.collect(Duration::from_millis(600)).await?;
        let mut seqs = Vec::new();
        let mut signals = 0;
        for (_, m) in got.iter().chain(after.iter()) {
            match &m.body {
                Some(Body::Audio(a)) if a.channel_id == 3 => seqs.push(a.seq),
                Some(Body::Signal(s)) if s.channel_id == 3 => {
                    assert!(s.snr_db.is_finite() && s.power_dbfs.is_finite());
                    signals += 1;
                }
                _ => {}
            }
        }
        // 50 frames/s, with a continuous sequence across the retune.
        assert!(seqs.len() >= 60, "{} frames in 1.6 s", seqs.len());
        for w in seqs.windows(2) {
            assert_eq!(w[1], w[0] + 1, "sequence gap: {seqs:?}");
        }
        assert!(signals >= 2, "{signals} ChannelSignal(s)");

        let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            subscription_id: 4,
            range: caps.tunable_range,
            bins: 1024,
            row_interval_ms: 100,
            quantization: None,
            compression: v1::WaterfallCompression::Zstd as i32,
        });
        let r = conn.call(sub, within).await?;
        let Some(Payload::WaterfallSubscribed(w)) = r.payload else {
            panic!("no WaterfallSubscribed: {r:?}");
        };
        let got = conn.collect(Duration::from_millis(1000)).await?;
        let mut decoder = WaterfallDecoder::new();
        let mut out = Vec::new();
        let mut rows = 0;
        for (_, m) in &got {
            if let Some(Body::Waterfall(row)) = &m.body {
                decoder
                    .decode(&row.payload, w.compression, w.bins, &mut out)
                    .expect("row decodes");
                assert_eq!(out.len(), 1024);
                rows += 1;
            }
        }
        assert!((7..=12).contains(&rows), "{rows} rows in 1 s at 10/s");
        let statuses: Vec<_> = got
            .iter()
            .filter_map(|(_, m)| match &m.body {
                Some(Body::Status(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let last = statuses.last().expect("a StationStatus");
        assert_eq!(
            last.backend.as_ref().map(|b| b.state),
            Some(v1::backend_health::State::Running as i32)
        );
        assert_eq!(
            last.frontend.as_ref().map(|f| f.sample_rate_hz),
            Some(24_000_000)
        );
        assert_eq!(last.active_channels, 1);
        assert_eq!(last.active_subscriptions, 1);
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await;
    if let Err(e) = result {
        panic!("{e}");
    }
    let _ = station.stop().await;
    // Everything was closed on disconnect.
    assert!(
        fake.wait_until(Duration::from_secs(1), |s| s.channels.is_empty())
            .await,
        "channels left in radiod"
    );
    fake.assert_client_contract();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_config_accepts_a_ka9q_backend() {
    let h = Harness::new(LocalPolicy::default()).await.expect("harness");
    let text = common::config_toml(&h.target).replace(
        "kind = \"fake\"",
        "kind = \"ka9q\"\n[backend.ka9q]\nstatus_group = \"239.77.0.6\"\n",
    );
    let path = h.target.work_dir.join("stationd.toml");
    std::fs::write(&path, text).expect("write");
    let out = Process::new(env!("CARGO_BIN_EXE_stationd"))
        .args(["check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("backend ka9q, status 239.77.0.6:5006, data from radiod"),
        "{stdout}"
    );
}

/// `stationd doctor`: radiod answers on its status group; the SDR check
/// reads USB IDs; every check line for a ka9q config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_checks_radiod_and_the_sdr() {
    use stationd::doctor::{self, Environment, Level};
    let fake = FakeRadiod::start().await;
    let detail = doctor::probe_radiod(fake.addr, Duration::from_secs(3))
        .await
        .expect("radiod answers");
    assert!(detail.contains("answered on"), "{detail}");
    // Nothing answers on another port.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    let e = doctor::probe_radiod(
        silent.local_addr().expect("addr"),
        Duration::from_millis(300),
    )
    .await
    .expect_err("silent");
    assert!(e.contains("radiod") || e.contains("no answer"), "{e}");

    let dir = tempfile::tempdir().expect("tmp");
    let config = dir.path().join("stationd.toml");
    std::fs::write(
        &config,
        format!(
            "state_dir = {:?}\n[sdr]\nprofile = \"rtlsdr\"\n[backend]\nkind = \"ka9q\"\n\
             [backend.ka9q]\nstatus_group = \"{}\"\ndata_group = \"127.0.0.1:5004\"\n",
            dir.path().join("state").display().to_string(),
            fake.addr
        ),
    )
    .expect("write");
    let usb = dir.path().join("usb");
    let env = |timeout| Environment {
        usb_devices: usb.clone(),
        now_unix_s: 0,
        timeout,
    };
    let level = |checks: &[doctor::Check], name: &str| {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} missing: {checks:?}"))
            .level
    };
    let checks = doctor::run(&config, &env(Duration::from_secs(3))).await;
    assert_eq!(level(&checks, "config"), Level::Pass, "{checks:?}");
    assert_eq!(level(&checks, "enrollment"), Level::Fail);
    assert_eq!(level(&checks, "sdr"), Level::Fail, "{checks:?}");
    assert_eq!(level(&checks, "radiod"), Level::Pass, "{checks:#?}");
    assert_eq!(level(&checks, "enroll"), Level::Skip);
    assert_eq!(level(&checks, "gateway"), Level::Skip);
    assert_eq!(level(&checks, "clock"), Level::Skip);
    assert!(doctor::failed(&checks));
    // Plug in an RTL-SDR.
    let port = usb.join("1-1.3");
    std::fs::create_dir_all(&port).expect("mkdir");
    std::fs::write(port.join("idVendor"), "0bda\n").expect("write");
    std::fs::write(port.join("idProduct"), "2838\n").expect("write");
    let checks = doctor::run(&config, &env(Duration::from_secs(3))).await;
    let sdr = checks.iter().find(|c| c.name == "sdr").expect("sdr");
    assert_eq!(sdr.level, Level::Pass, "{sdr}");
    assert!(sdr.detail.contains("0bda:2838 at 1-1.3"), "{sdr}");
    // A broken config: one failing line.
    std::fs::write(&config, "nope").expect("write");
    let checks = doctor::run(&config, &env(Duration::from_secs(1))).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].level, Level::Fail);
    fake.assert_client_contract();
}
