// SPDX-License-Identifier: GPL-3.0-only
//! The `stationd` command line.
//!
//! ```text
//! stationd run [--config /etc/coaxhub/stationd.toml]
//! stationd check-config [--config /etc/coaxhub/stationd.toml]
//! stationd --version
//! ```
//!
//! Exit codes: [`EXIT_OK`], [`EXIT_FAILURE`], [`EXIT_CONFIG`] (also clap
//! usage errors), [`EXIT_REVOKED`].
//!
//! `enroll`, `status`, `doctor` and `unenroll` arrive with enrollment.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use station_backend::{Backend, FakeBackend};
use tracing::{error, info, warn};

use crate::config::{BackendKind, Config, DEFAULT_CONFIG_PATH};
use crate::daemon::{RunError, RunOptions};
use crate::tls;

/// Clean shutdown.
pub const EXIT_OK: u8 = 0;
/// A runtime failure.
pub const EXIT_FAILURE: u8 = 1;
/// Invalid configuration or credentials (or command-line usage).
pub const EXIT_CONFIG: u8 = 2;
/// The hub revoked the station's certificate: re-enroll.
pub const EXIT_REVOKED: u8 = 3;

/// The `CoaxHub` station daemon.
#[derive(Debug, Parser)]
#[command(name = "stationd", version, about, long_about = None)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// `stationd` subcommands.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Run the daemon: connect to the hub and serve it until SIGTERM/SIGINT.
    Run {
        /// Path to stationd.toml.
        #[arg(long, short = 'c', default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Validate stationd.toml and the TLS credentials it names, then exit.
    CheckConfig {
        /// Path to stationd.toml.
        #[arg(long, short = 'c', default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

/// Why a command failed, with its exit code.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// Configuration or credentials.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// TLS credentials.
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    /// The daemon failed.
    #[error(transparent)]
    Run(#[from] RunError),
    /// Signal handling could not be set up.
    #[error("cannot install signal handlers: {0}")]
    Signals(std::io::Error),
}

impl CommandError {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) | Self::Tls(_) | Self::Run(RunError::Tls(_)) => EXIT_CONFIG,
            Self::Run(RunError::Revoked) => EXIT_REVOKED,
            Self::Run(_) | Self::Signals(_) => EXIT_FAILURE,
        }
    }
}

/// The backend named in the config.
pub fn backend_for(kind: BackendKind) -> Arc<dyn Backend> {
    match kind {
        BackendKind::Fake => Arc::new(FakeBackend::default()),
    }
}

/// `check-config`: loads the config and credentials; returns a one-line
/// summary.
pub fn check_config(path: &Path) -> Result<String, CommandError> {
    let config = Config::load(path)?;
    let tls = tls::load(&config)?;
    if let Some(w) = config.key_permission_warning() {
        warn!("{w}");
    }
    Ok(format!(
        "config OK: station {} -> {} (backend {}, {} trust anchor(s), max_channels {}, max_waterfall_subscriptions {}, frontend control {}, admin ops [{}])",
        tls.station_id,
        config.gateway.url,
        config.backend,
        tls.trust_anchors,
        config.policy.max_channels,
        config.policy.max_waterfall_subscriptions,
        if config.policy.allow_frontend_control {
            "allowed"
        } else {
            "denied"
        },
        config
            .policy
            .allowed_admin_ops
            .iter()
            .map(station_proto::v1::AdminOpKind::as_str_name)
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

/// `run`: starts the daemon and serves until `stop` resolves (the binary
/// passes SIGTERM/SIGINT), then shuts down gracefully.
pub async fn run(
    path: &Path,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), CommandError> {
    let config = Config::load(path)?;
    if let Some(w) = config.key_permission_warning() {
        warn!("{w}");
    }
    let mut handle =
        crate::daemon::spawn(&config, backend_for(config.backend), RunOptions::default())?;
    tokio::select! {
        result = handle.join() => return result.map_err(CommandError::from),
        () = stop => info!("stop requested"),
    }
    handle.shutdown();
    handle.join().await.map_err(CommandError::from)
}

/// Resolves on the first SIGTERM or SIGINT.
pub fn termination_signal() -> Result<impl std::future::Future<Output = ()>, CommandError> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).map_err(CommandError::Signals)?;
    let mut int = signal(SignalKind::interrupt()).map_err(CommandError::Signals)?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    })
}

/// Maps a command result to the process exit code, logging the error.
pub fn exit_code(result: Result<(), CommandError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(e) => {
            error!(error = %e, "stationd failed");
            ExitCode::from(e.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_subcommands_and_defaults() {
        let c = Cli::try_parse_from(["stationd", "run", "--config", "/tmp/s.toml"]).expect("run");
        assert_eq!(
            c.command,
            Command::Run {
                config: "/tmp/s.toml".into()
            }
        );
        let c = Cli::try_parse_from(["stationd", "run"]).expect("run");
        assert_eq!(
            c.command,
            Command::Run {
                config: DEFAULT_CONFIG_PATH.into()
            }
        );
        let c = Cli::try_parse_from(["stationd", "check-config", "-c", "x.toml"]).expect("check");
        assert_eq!(
            c.command,
            Command::CheckConfig {
                config: "x.toml".into()
            }
        );
        let e = Cli::try_parse_from(["stationd", "--version"]).expect_err("version exits");
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(e.to_string().contains(env!("CARGO_PKG_VERSION")));
        let e = Cli::try_parse_from(["stationd"]).expect_err("subcommand required");
        assert_eq!(e.exit_code(), i32::from(EXIT_CONFIG));
        assert!(Cli::try_parse_from(["stationd", "enroll"]).is_err());
    }

    #[test]
    fn exit_codes() {
        let revoked = CommandError::Run(RunError::Revoked);
        assert_eq!(revoked.exit_code(), EXIT_REVOKED);
        let task = CommandError::Run(RunError::Task("x".into()));
        assert_eq!(task.exit_code(), EXIT_FAILURE);
        let tls = CommandError::Tls(tls::TlsError::ServerName("x".into()));
        assert_eq!(tls.exit_code(), EXIT_CONFIG);
        let missing = check_config(Path::new("/nonexistent/stationd.toml")).expect_err("missing");
        assert_eq!(missing.exit_code(), EXIT_CONFIG);
        assert_eq!(exit_code(Ok(())), ExitCode::from(EXIT_OK));
        assert_eq!(exit_code(Err(revoked)), ExitCode::from(EXIT_REVOKED));
        let sig = CommandError::Signals(std::io::Error::other("x"));
        assert_eq!(sig.exit_code(), EXIT_FAILURE);
    }
}
