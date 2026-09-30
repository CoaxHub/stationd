// SPDX-License-Identifier: GPL-3.0-only
//! Runs stationd in-process as a conformance `StationUnderTest`.
#![allow(dead_code, clippy::missing_panics_doc)]
// Debug formatting quotes paths as TOML strings on purpose.
#![allow(clippy::unnecessary_debug_formatting)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use station_backend::{Backend, FakeBackend};
use station_conformance::{BoxError, Exit, StationUnderTest, Target};
use station_proto::backoff::Policy;
use stationd::{Config, DaemonHandle, RunError, RunOptions};

/// Short intervals matching `ScenarioConfig::fast()`.
pub fn fast_options() -> RunOptions {
    RunOptions {
        backoff: Policy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(300),
            reset_after: Duration::from_secs(300),
            disabled_retry: Duration::from_millis(700),
            unsupported_version_retry: Duration::from_millis(900),
        },
        connect_timeout: Duration::from_secs(3),
        welcome_timeout: Duration::from_secs(3),
        keepalive_interval: Duration::from_millis(300),
        keepalive_timeout: Duration::from_millis(300),
        shutdown_grace: Duration::from_secs(1),
        ..RunOptions::default()
    }
}

/// The `stationd.toml` for `target`.
pub fn config_toml(target: &Target) -> String {
    let ops: Vec<String> = target
        .policy
        .admin_op_names()
        .iter()
        .map(|n| format!("{n:?}"))
        .collect();
    format!(
        "gateway_url = {:?}\ncert = {:?}\nkey = {:?}\ngateway_ca = {:?}\n\
         allow_frontend_control = {}\nallowed_admin_ops = [{}]\nmax_channels = {}\n\
         max_waterfall_subscriptions = {}\n[backend]\nkind = \"fake\"\n",
        target.gateway_url,
        target.cert,
        target.key,
        target.gateway_ca,
        target.policy.allow_frontend_control,
        ops.join(", "),
        target.policy.max_channels,
        target.policy.max_waterfall_subscriptions,
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

/// stationd running in this process with a `FakeBackend` (the default)
/// or any other backend.
pub struct InProcess {
    pub handle: Option<DaemonHandle>,
    pub backend: Arc<dyn Backend>,
    exit: Option<Exit>,
}

impl Default for InProcess {
    fn default() -> Self {
        Self::with_backend(Arc::new(FakeBackend::default()))
    }
}

impl InProcess {
    /// stationd driving `backend` (the config file still says `fake`: the
    /// in-process daemon takes the backend directly).
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self {
            handle: None,
            backend,
            exit: None,
        }
    }
}

#[async_trait::async_trait]
impl StationUnderTest for InProcess {
    async fn start(&mut self, target: &Target) -> Result<(), BoxError> {
        let config = Config::load(&write_config(target))?;
        let handle = stationd::spawn(&config, Arc::clone(&self.backend), fast_options())?;
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
