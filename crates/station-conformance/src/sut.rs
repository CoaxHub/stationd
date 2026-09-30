// SPDX-License-Identifier: MIT
//! The station under test: how the conformance scenarios start and stop a
//! station implementation.

use std::path::PathBuf;
use std::time::Duration;

use station_proto::v1;
use uuid::Uuid;

use crate::error::BoxError;

/// The local owner policy the station must apply for a scenario (the
/// `stationd.toml` settings of station-protocol §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPolicy {
    /// Whether `SetFrontend` is allowed.
    pub allow_frontend_control: bool,
    /// Admin operations the hub may trigger.
    pub allowed_admin_ops: Vec<v1::AdminOpKind>,
    /// Hard cap on audio channels.
    pub max_channels: u32,
    /// Hard cap on waterfall subscriptions.
    pub max_waterfall_subscriptions: u32,
}

impl Default for LocalPolicy {
    /// The §7 defaults.
    fn default() -> Self {
        Self {
            allow_frontend_control: true,
            allowed_admin_ops: Vec::new(),
            max_channels: 16,
            max_waterfall_subscriptions: 4,
        }
    }
}

impl LocalPolicy {
    /// The admin-op names used in `stationd.toml` (`restart_stationd`, ...).
    pub fn admin_op_names(&self) -> Vec<&'static str> {
        self.allowed_admin_ops
            .iter()
            .map(|k| match k {
                v1::AdminOpKind::RestartStationd => "restart_stationd",
                v1::AdminOpKind::RestartBackend => "restart_backend",
                v1::AdminOpKind::Update => "update",
                v1::AdminOpKind::RebootHost => "reboot_host",
                v1::AdminOpKind::Unspecified => "unspecified",
            })
            .collect()
    }
}

/// Where and how the station under test must connect.
#[derive(Debug, Clone)]
pub struct Target {
    /// The gateway URL, `https://localhost:<port>`.
    pub gateway_url: String,
    /// The CA bundle (PEM file) to trust for the gateway, and nothing else.
    pub gateway_ca: PathBuf,
    /// The station's client certificate chain (PEM file).
    pub cert: PathBuf,
    /// The station's private key (PEM file, mode 0600).
    pub key: PathBuf,
    /// The station ID in `cert`.
    pub station_id: Uuid,
    /// The local policy to apply.
    pub policy: LocalPolicy,
    /// A scratch directory for the station (e.g. its config file); removed
    /// after the scenario.
    pub work_dir: PathBuf,
}

/// How a station process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// Whether it exited successfully (exit status 0).
    pub success: bool,
    /// A description (exit code, error).
    pub detail: String,
}

/// A station implementation driven by the scenarios.
///
/// The station must reconnect by itself (station-protocol §2.2); scenarios
/// never restart it. Implementations for `stationd` run it in-process or as
/// a child process.
#[async_trait::async_trait]
pub trait StationUnderTest: Send {
    /// Starts the station against `target`. Returns once it is started, not
    /// necessarily connected.
    async fn start(&mut self, target: &Target) -> Result<(), BoxError>;

    /// Requests a graceful shutdown (SIGTERM) and waits for the exit. Must
    /// also work, and return the stored exit, if the station already exited.
    async fn stop(&mut self) -> Result<Exit, BoxError>;

    /// Waits up to `within` for the station to exit on its own; `None` if it
    /// is still running.
    async fn wait_exit(&mut self, within: Duration) -> Result<Option<Exit>, BoxError>;
}
