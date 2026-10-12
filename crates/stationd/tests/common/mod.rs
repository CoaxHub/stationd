// SPDX-License-Identifier: GPL-3.0-only
//! Runs stationd in-process as a conformance `StationUnderTest`.
#![allow(dead_code, clippy::missing_panics_doc)]
// Debug formatting quotes paths as TOML strings on purpose.
#![allow(clippy::unnecessary_debug_formatting)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use station_backend::Backend;
use station_conformance::{BoxError, Exit, StationUnderTest, Target};
use station_proto::backoff::Policy;
use stationd::receivers::{Driver, FakeBackends, FakeUnits, ReceiverOptions, SharedBackend};
use stationd::{Config, DaemonHandle, RunError, RunOptions};

/// Short intervals matching `ScenarioConfig::fast()`.
pub fn fast_options() -> RunOptions {
    fast_options_scaled(60)
}

/// [`fast_options`] with measuring on a clock `scale` times as fast.
pub fn fast_options_scaled(scale: u32) -> RunOptions {
    RunOptions {
        backoff: Policy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(300),
            reset_after: Duration::from_secs(300),
            // The wider first window after a dropped session, scaled like
            // the rest.
            after_drop_max: Duration::from_millis(250),
            disabled_retry: Duration::from_millis(700),
            unsupported_version_retry: Duration::from_millis(900),
        },
        connect_timeout: Duration::from_secs(3),
        welcome_timeout: Duration::from_secs(3),
        keepalive_interval: Duration::from_millis(300),
        keepalive_timeout: Duration::from_millis(300),
        shutdown_grace: Duration::from_secs(1),
        // Measuring runs on a clock 60 times as fast (ScenarioConfig::fast's
        // one-second minute).
        measure_clock: Arc::new(stationd::measure::ScaledClock::new(
            stationd::measure::clock::system_now_ms(),
            scale,
        )),
        // An idle host: measuring never pauses for the load of the machine
        // running the tests (station protocol §4.1).
        measure_load: Arc::new(stationd::measure::FixedLoad::idle()),
        // Units and SDRs looked at often (ScenarioConfig::fast's 3 s).
        receivers: ReceiverOptions {
            poll: Duration::from_millis(20),
            hardware_poll: Duration::from_millis(50),
        },
        ..RunOptions::default()
    }
}

/// The `stationd.toml` for `target`: static `[credentials]`, the fake
/// backend on the target's simulated SDRs (development builds only), state
/// under the work dir.
pub fn config_toml(target: &Target) -> String {
    format!(
        "state_dir = {:?}\n\
         [credentials]\ngateway_url = {:?}\ncert = {:?}\nkey = {:?}\ngateway_ca = {:?}\n\
         [backend]\nkind = \"fake\"\nusb_devices = {:?}\n",
        target.work_dir.join("state"),
        target.gateway_url,
        target.cert,
        target.key,
        target.gateway_ca,
        target.usb_devices,
    )
}

/// Writes the config for `target` into its work dir; returns the path.
pub fn write_config(target: &Target) -> PathBuf {
    let path = target.work_dir.join("stationd.toml");
    std::fs::write(&path, config_toml(target)).expect("write config");
    path
}

pub fn load(path: &Path) -> Config {
    Config::load(path).expect("valid config")
}

/// Fake backends and units, sharing nothing with other tests.
pub fn fake_driver() -> (Driver, FakeBackends, FakeUnits) {
    let backends = FakeBackends::default();
    let units = FakeUnits::default();
    let driver = Driver {
        backends: Arc::new(backends.clone()),
        units: Arc::new(units.clone()),
    };
    (driver, backends, units)
}

fn exit_of(result: Result<(), RunError>) -> Exit {
    match result {
        Ok(()) => Exit {
            success: true,
            detail: "stopped".into(),
        },
        Err(e) => Exit {
            success: false,
            detail: e.to_string(),
        },
    }
}

/// stationd running in this process with fake backends and units (the
/// default) or one given backend for every receiver. The driver outlives a
/// restart of the daemon, as the units of a station outlive stationd.
pub struct InProcess {
    pub handle: Option<DaemonHandle>,
    pub driver: Driver,
    /// How much faster than real time measuring runs (60 by default).
    pub measure_scale: u32,
    /// The 1-minute load average measuring's pause rule sees (0 by default,
    /// whatever the machine's load), on this machine's CPUs.
    pub host_load: f32,
    exit: Option<Exit>,
}

impl Default for InProcess {
    fn default() -> Self {
        Self::with_driver(fake_driver().0)
    }
}

impl InProcess {
    /// stationd running its receivers with `driver`.
    pub fn with_driver(driver: Driver) -> Self {
        Self {
            handle: None,
            driver,
            measure_scale: 60,
            host_load: 0.0,
            exit: None,
        }
    }

    /// stationd with `backend` for every receiver (the config file still
    /// says `fake`: the in-process daemon takes the driver directly).
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self::with_driver(Driver {
            backends: Arc::new(SharedBackend(backend)),
            units: Arc::new(FakeUnits::default()),
        })
    }
}

#[async_trait::async_trait]
impl StationUnderTest for InProcess {
    async fn start(&mut self, target: &Target) -> Result<(), BoxError> {
        let config = Config::load(&write_config(target))?;
        let options = RunOptions {
            measure_load: Arc::new(stationd::measure::FixedLoad::on_this_host(self.host_load)),
            ..fast_options_scaled(self.measure_scale)
        };
        let handle = stationd::spawn(&config, self.driver.clone(), options)?;
        assert_eq!(handle.station_id(), target.station_id);
        self.handle = Some(handle);
        self.exit = None;
        Ok(())
    }

    async fn stop(&mut self) -> Result<Exit, BoxError> {
        if let Some(mut h) = self.handle.take() {
            h.shutdown();
            let result = tokio::time::timeout(Duration::from_secs(10), h.join())
                .await
                .map_err(|_| "stationd did not stop within 10 s")?;
            self.exit = Some(exit_of(result));
        }
        self.exit.clone().ok_or_else(|| "never started".into())
    }

    async fn wait_exit(&mut self, within: Duration) -> Result<Option<Exit>, BoxError> {
        let Some(h) = self.handle.as_mut() else {
            return Ok(self.exit.clone());
        };
        match tokio::time::timeout(within, h.join()).await {
            Ok(result) => {
                self.handle = None;
                self.exit = Some(exit_of(result));
                Ok(self.exit.clone())
            }
            Err(_) => Ok(None),
        }
    }
}
