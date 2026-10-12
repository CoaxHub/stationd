// SPDX-License-Identifier: GPL-3.0-only
//! Every station-conformance scenario against the real stationd, in-process,
//! over real mTLS on loopback, with fake backends and units on simulated
//! SDRs.

// Static [credentials] and [backend] are development-only settings.
#![cfg(not(feature = "packaged"))]

mod common;

use station_conformance::scenarios::{self, ScenarioConfig};

async fn run(name: &str) {
    let scenario = scenarios::by_name(name).expect("known scenario");
    let mut station = common::InProcess::default();
    let cfg = ScenarioConfig::fast();
    if let Err(e) = (scenario.run)(&mut station, &cfg).await {
        panic!("{name}: {e}");
    }
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

/// The pause rule still holds with the injected load (station protocol
/// §4.1): on a host whose load stays at 0.75 × its CPUs, an idle plan
/// measures nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_host_measures_nothing() {
    let scenario = scenarios::by_name("measures_when_asked").expect("known scenario");
    let mut station = common::InProcess::default();
    #[allow(clippy::cast_precision_loss, reason = "CPU counts are small")]
    let busy = 0.75 * stationd::measure::FixedLoad::idle().0.cpu_count.max(1) as f32;
    station.host_load = busy;
    let cfg = ScenarioConfig::fast();
    let e = (scenario.run)(&mut station, &cfg)
        .await
        .expect_err("a busy station must not report");
    assert!(
        e.to_string().contains("no NoiseFloorReport"),
        "unexpected failure: {e}"
    );
}
