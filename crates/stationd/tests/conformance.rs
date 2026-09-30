// SPDX-License-Identifier: GPL-3.0-only
//! Every station-conformance scenario against the real stationd, in-process,
//! over real mTLS on loopback, with a `FakeBackend`.

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
