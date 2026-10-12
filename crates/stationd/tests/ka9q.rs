// SPDX-License-Identifier: GPL-3.0-only
//! stationd with the ka9q backend (`Ka9qBackend`) against fake radiods of
//! `backend-ka9q/tests/support`, one per receiver as on a station, in real
//! time over loopback:
//!
//! - every station-conformance scenario, with the fake hub validating each
//!   message with the §5 rules;
//! - a scripted session that checks the media in detail (audio sequence
//!   across a retune, waterfall rows, S-meter readings);
//! - `stationd check-config` and `stationd doctor` on a `[backend] kind =
//!   "ka9q"` config.

// Static [credentials] and [backend] are development-only settings.
#![cfg(not(feature = "packaged"))]
#![allow(
    clippy::too_many_lines,
    reason = "end-to-end scripts read top to bottom"
)]

mod common;
#[path = "../../backend-ka9q/tests/support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use backend_ka9q::backend::{Ka9qBackend, Ka9qConfig};
use station_backend::{Backend, BackendError};
use station_conformance::StationUnderTest as _;
use station_conformance::scenarios::{self, After, Harness, ScenarioConfig, run_scenario};
use station_proto::v1;
use station_proto::v1::request::Command;
use station_proto::v1::response::Payload;
use station_proto::v1::station_message::Body;
use station_proto::validate::WaterfallDecoder;
use stationd::cli::ConfigArg;
use stationd::receivers::{BackendFactory, Driver, FakeUnits, StoredReceiver};
use support::FakeRadiod;

/// A fake radiod whose front end is a 2.048 MS/s complex RTL-SDR at
/// `center_hz` that retunes.
async fn radiod_at(center_hz: f64) -> Arc<FakeRadiod> {
    let fake = FakeRadiod::start().await;
    {
        let mut st = fake.state();
        st.fe_lo = center_hz;
        st.fe_low = -1_024_000.0;
        st.fe_high = 1_024_000.0;
        st.fe_isreal = false;
        st.fe_tunable = true;
    }
    Arc::new(fake)
}

/// The radiod of each receiver of the standard set: receiver 1's at
/// 7.1 MHz, receiver 2's at 14.2 MHz. Each backend the station makes is a
/// new `Ka9qBackend` on its receiver's radiod.
#[derive(Debug, Clone)]
struct Radiods(HashMap<u32, Arc<FakeRadiod>>);

impl Radiods {
    async fn standard() -> Self {
        Self(HashMap::from([
            (1, radiod_at(7_100_000.0).await),
            (2, radiod_at(14_200_000.0).await),
        ]))
    }

    fn assert_client_contract(&self) {
        for fake in self.0.values() {
            fake.assert_client_contract();
        }
    }
}

impl BackendFactory for Radiods {
    fn info(&self) -> v1::BackendInfo {
        backend_ka9q::backend::backend_info()
    }

    fn backend(&self, rx: &StoredReceiver) -> Result<Arc<dyn Backend>, BackendError> {
        let fake = self
            .0
            .get(&rx.receiver_id)
            .cloned()
            .ok_or_else(|| BackendError::backend("no radiod for this receiver"))?;
        let config = Ka9qConfig::new(fake.addr, Some("127.0.0.1:0".parse().expect("addr")));
        let backend = Arc::new(Ka9qBackend::new(config)?);
        // The fake radiod sends to this client's sockets.
        let wire = Arc::clone(&backend);
        tokio::spawn(async move {
            if let Ok(client) = wire.client().await {
                fake.set_rtp_dest(client.local_data_addr());
                if let Some(status) = client.local_channel_status_addr() {
                    fake.set_status_dest(status);
                }
            }
        });
        Ok(backend)
    }

    fn can_measure(&self) -> bool {
        true
    }
}

fn station(radiods: &Radiods) -> common::InProcess {
    common::InProcess::with_driver(Driver {
        backends: Arc::new(radiods.clone()),
        units: Arc::new(FakeUnits::default()),
    })
}

async fn run(name: &str) {
    let scenario = scenarios::by_name(name).expect("known scenario");
    let radiods = Radiods::standard().await;
    let mut station = station(&radiods);
    let cfg = ScenarioConfig {
        // Each poll is a round trip to the fake radiod in real time, which the
        // measuring clock, 60 times as fast, outruns once a listener's audio flows
        // too: the scenarios that wait for reports are left to the fake backend
        // (conformance.rs) and to the run against radiod on the signal generator;
        // the ka9q measurement channel has its own tests (backend-ka9q
        // tests/backend.rs).
        measure_minute: None,
        ..ScenarioConfig::fast()
    };
    if let Err(e) = (scenario.run)(&mut station, &cfg).await {
        panic!("{name} with the ka9q backend: {e}");
    }
    radiods.assert_client_contract();
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
    admin_op_ok_or_unsupported,
    set_frontend_on_a_receiver,
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
    channel_limit_per_receiver,
    subscribes_waterfall_and_streams_rows,
    drops_channels_on_disconnect,
    reports_hardware,
    applies_receiver_set,
    refuses_invalid_receiver_sets,
    applies_set_naming_an_absent_sdr,
    unchanged_set_keeps_channels,
    changed_and_removed_receivers,
    sdr_unplugged_and_back,
    receivers_survive_reconnect_and_restart,
    measures_when_asked,
    duty_follows_listeners,
    stops_measuring_on_frontend_change,
    one_plan_per_connection,
);

fn ok(r: &v1::Response) -> bool {
    r.result
        .as_ref()
        .is_some_and(|res| res.code == v1::ResultCode::Ok as i32)
}

/// A full session through the tunnel on receiver 1: capabilities, a channel
/// with a retune, a waterfall, its radiod's health in `StationStatus`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_through_the_tunnel() {
    let radiods = Radiods::standard().await;
    let mut station = station(&radiods);
    let cfg = ScenarioConfig::fast();
    let harness = Harness::new().await;
    let result = run_scenario(&mut station, &cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(&cfg).await?;
        let within = cfg.response_within;
        let rx = &caps.receivers[0];
        assert_eq!(rx.receiver_id, 1);
        assert_eq!(rx.sample_rate_hz, 24_000_000);
        assert_eq!(rx.sdr_model, "fake radiod");
        assert_eq!(rx.max_channels, 16, "the package's claim caps radiod's 64");
        assert_eq!(
            rx.tunable_range,
            Some(v1::FrequencyRange {
                start_hz: 6_076_000,
                end_hz: 8_124_000
            })
        );

        let open = Command::OpenChannel(v1::OpenChannel {
            channel_id: 3,
            receiver_id: 1,
            frequency_hz: 7_100_000,
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
            frequency_hz: Some(7_090_000),
            ..Default::default()
        });
        let r = conn.call(retune, within).await?;
        let Some(Payload::ChannelUpdated(u)) = r.payload else {
            panic!("no ChannelUpdated: {r:?}");
        };
        assert_eq!(u.frequency_hz, 7_090_000);
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
            receiver_id: 1,
            range: rx.tunable_range,
            bins: 1024,
            row_interval_ms: 100,
            quantization: None,
            compression: v1::WaterfallCompression::Zstd as i32,
        });
        let r = conn.call(sub, within).await?;
        let Some(Payload::WaterfallSubscribed(w)) = r.payload else {
            panic!("no WaterfallSubscribed: {r:?}");
        };
        assert_eq!(w.receiver_id, 1);
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
        let one = last
            .receivers
            .iter()
            .find(|r| r.receiver_id == 1)
            .expect("receiver 1's health");
        assert_eq!(one.state, v1::ReceiverState::Running as i32);
        assert_eq!(
            one.backend.as_ref().map(|b| b.state),
            Some(v1::backend_health::State::Running as i32)
        );
        assert_eq!(
            one.frontend.as_ref().map(|f| f.sample_rate_hz),
            Some(24_000_000)
        );
        assert_eq!(one.active_channels, 1);
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
        radiods.0[&1]
            .wait_until(Duration::from_secs(1), |s| s.channels.is_empty())
            .await,
        "channels left in radiod"
    );
    radiods.assert_client_contract();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_config_accepts_a_ka9q_backend() {
    let h = Harness::new().await.expect("harness");
    let text = common::config_toml(&h.target).replace("kind = \"fake\"", "kind = \"ka9q\"")
        + "[backend.ka9q]\nstatus_group = \"239.77.0.6\"\n";
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
        stdout.contains("backend ka9q, your radiod at status 239.77.0.6:5006, data from radiod"),
        "{stdout}"
    );
}

/// `stationd doctor`: radiod answers on its status group; the SDRs and the
/// receivers from sysfs and receivers.json; every check line for a ka9q
/// config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_checks_radiod_the_sdrs_and_the_receivers() {
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
            "state_dir = {:?}\n[backend]\nkind = \"ka9q\"\n\
             [backend.ka9q]\nstatus_group = \"{}\"\ndata_group = \"127.0.0.1:5004\"\n",
            dir.path().join("state").display().to_string(),
            fake.addr
        ),
    )
    .expect("write");
    let receivers = dir.path().join("state/receivers");
    std::fs::create_dir_all(&receivers).expect("mkdir");
    std::fs::write(
        receivers.join("receivers.json"),
        r#"{"receivers": [{"receiver_id": 4, "slot": 0, "sdr_serial": "00000001",
            "model": "rtlsdr_other", "profile": "rtlsdr", "center_frequency_hz": 98000000}]}"#,
    )
    .expect("write");
    let usb = dir.path().join("usb");
    let env = |timeout| Environment {
        usb_devices: Some(usb.clone()),
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
    let arg = ConfigArg::at(&config);
    let checks = doctor::run(&arg, &env(Duration::from_secs(3))).await;
    assert_eq!(level(&checks, "config"), Level::Pass, "{checks:?}");
    assert_eq!(level(&checks, "enrollment"), Level::Fail);
    assert_eq!(level(&checks, "sdrs"), Level::Warn, "{checks:?}");
    assert_eq!(level(&checks, "receivers"), Level::Warn, "{checks:#?}");
    assert_eq!(level(&checks, "radiod"), Level::Pass, "{checks:#?}");
    assert_eq!(level(&checks, "enroll"), Level::Skip);
    assert_eq!(level(&checks, "gateway"), Level::Skip);
    assert_eq!(level(&checks, "clock"), Level::Skip);
    assert!(doctor::failed(&checks));
    // Plug in its RTL-SDR.
    let port = usb.join("1-1.3");
    std::fs::create_dir_all(&port).expect("mkdir");
    std::fs::write(port.join("idVendor"), "0bda\n").expect("write");
    std::fs::write(port.join("idProduct"), "2838\n").expect("write");
    std::fs::write(port.join("serial"), "00000001\n").expect("write");
    let checks = doctor::run(&arg, &env(Duration::from_secs(3))).await;
    let sdr = checks.iter().find(|c| c.name == "sdrs").expect("sdrs");
    assert_eq!(sdr.level, Level::Pass, "{sdr}");
    assert!(sdr.detail.contains("(0bda:2838"), "{sdr}");
    assert!(sdr.detail.contains("at USB port 1-1.3"), "{sdr}");
    assert_eq!(level(&checks, "receivers"), Level::Pass, "{checks:#?}");
    // A broken config: one failing line.
    std::fs::write(&config, "nope").expect("write");
    let checks = doctor::run(&arg, &env(Duration::from_secs(1))).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].level, Level::Fail);
    fake.assert_client_contract();
}
