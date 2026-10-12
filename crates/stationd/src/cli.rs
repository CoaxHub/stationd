// SPDX-License-Identifier: GPL-3.0-only
//! The `stationd` command line.
//!
//! ```text
//! stationd run
//! stationd enroll         [--token TOKEN] [--force]
//! stationd status         [--json]
//! stationd doctor
//! stationd unenroll
//! stationd backend-config --receiver ID [--out PATH|-] [--templates DIR]
//! stationd --version
//! ```
//!
//! A packaged build reads no settings file (station-protocol §7). A
//! development build takes `--config PATH` on every command (default
//! `/etc/coaxhub/stationd.toml`, [`crate::config`]) and has one more:
//!
//! ```text
//! stationd check-config   [--config PATH]
//! ```
//!
//! Exit codes: [`EXIT_OK`], [`EXIT_FAILURE`], [`EXIT_CONFIG`] (also clap
//! usage errors), [`EXIT_REVOKED`] / [`EXIT_NOT_ENROLLED`] (both mean:
//! enroll this station), [`EXIT_TOKEN`] (`enroll`: get a new token and try
//! again).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tracing::{error, info, warn};

use crate::backend_config::{self, RenderError};
use crate::config::{BackendConfig, Config, CredentialsMode};
use crate::credentials::{self, CredentialsError};
use crate::daemon::{RunError, RunOptions};
use crate::enroll::{self, EnrollError, EnrollOptions};
use crate::local_status::{self, format_duration, format_utc, unix_now};
use crate::receivers::store::StoreError;
use crate::receivers::{Driver, ExternalUnits, FakeBackends, FakeUnits, Ka9qBackends};
use crate::state::{StateDir, StateError};
use crate::{doctor, tls};

/// Clean shutdown.
pub const EXIT_OK: u8 = 0;
/// A runtime failure (or a failed `doctor` check).
pub const EXIT_FAILURE: u8 = 1;
/// Invalid configuration or credentials (or command-line usage).
pub const EXIT_CONFIG: u8 = 2;
/// The station's certificate was revoked (`Goodbye{REVOKED}`, or
/// `revoked` from `station-api`): re-enroll. The service unit does not
/// restart on it.
pub const EXIT_REVOKED: u8 = 3;
/// The station is not enrolled: enroll it (the same code as
/// [`EXIT_REVOKED`]).
pub const EXIT_NOT_ENROLLED: u8 = 3;
/// `enroll`: the token was refused (invalid, expired or already used),
/// malformed or empty. Another token may succeed; the installer asks for
/// one again on this code.
pub const EXIT_TOKEN: u8 = 4;

/// The `CoaxHub` station daemon.
#[derive(Debug, Parser)]
#[command(name = "stationd", version, about, long_about = None)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Where the settings come from: built in (packaged builds), or
/// `--config` (development builds).
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConfigArg {
    /// Path to stationd.toml (development builds).
    #[cfg(not(feature = "packaged"))]
    #[arg(long, short = 'c', default_value = crate::config::DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,
}

impl ConfigArg {
    /// The development config at `path`.
    #[cfg(not(feature = "packaged"))]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            config: path.into(),
        }
    }

    /// What this build runs with.
    pub fn load(&self) -> Result<Config, CommandError> {
        #[cfg(not(feature = "packaged"))]
        {
            Ok(Config::load(&self.config)?)
        }
        #[cfg(feature = "packaged")]
        {
            let built_in =
                crate::flavour::Flavour::current()
                    .built_in
                    .ok_or(CommandError::NotApplicable(
                        "this build has no built-in domain",
                    ))?;
            Ok(Config::built_in(built_in)?)
        }
    }

    /// How it is shown in messages.
    pub fn describe(&self) -> String {
        #[cfg(not(feature = "packaged"))]
        {
            self.config.display().to_string()
        }
        #[cfg(feature = "packaged")]
        {
            "the built-in settings".to_owned()
        }
    }
}

/// `stationd` subcommands.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Run the daemon: start the receivers, connect to the hub and serve it
    /// until SIGTERM/SIGINT.
    Run {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
    },
    /// Enroll this station with a one-time token from the web app.
    Enroll {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
        /// The token (`chs_enr_…`). Without it, stationd prompts (input
        /// hidden) or reads one line from standard input.
        #[arg(long)]
        token: Option<String>,
        /// Enroll again even if this station is already enrolled.
        #[arg(long)]
        force: bool,
    },
    /// Show enrollment, certificate expiry, home hub, the receivers and the
    /// daemon's state.
    Status {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check the enrollment, the SDRs, the receivers, the network and the
    /// clock.
    Doctor {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
    },
    /// Delete this station's credentials and assignment (keeps the applied
    /// receivers).
    Unenroll {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
    },
    /// Render one receiver's radiod config from receivers.json (its unit
    /// runs this before every start).
    BackendConfig {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
        /// The receiver.
        #[arg(long)]
        receiver: u32,
        /// Where to write it (`-` for standard output; default
        /// /run/coaxhub-radiod/<receiver>/radiod.conf).
        #[arg(long)]
        out: Option<PathBuf>,
        /// The template directory (default: the package's).
        #[arg(long)]
        templates: Option<PathBuf>,
    },
    /// Validate stationd.toml, the credentials and the applied receivers,
    /// then exit (development builds).
    #[cfg(not(feature = "packaged"))]
    CheckConfig {
        #[command(flatten)]
        #[allow(missing_docs)]
        config: ConfigArg,
    },
}

/// Why a command failed, with its exit code.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// Configuration.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// TLS credentials.
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    /// Credentials.
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    /// The daemon failed.
    #[error(transparent)]
    Run(#[from] RunError),
    /// Signal handling could not be set up.
    #[error("cannot install signal handlers: {0}")]
    Signals(std::io::Error),
    /// Enrollment failed.
    #[error(transparent)]
    Enroll(#[from] EnrollError),
    /// The radiod config could not be rendered.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// The state directory could not be changed.
    #[error(transparent)]
    State(#[from] StateError),
    /// The command does not apply to this configuration.
    #[error("{0}")]
    NotApplicable(&'static str),
    /// Output could not be written.
    #[error("cannot write the output: {0}")]
    Output(std::io::Error),
    /// `doctor` found a failure (already printed).
    #[error("some checks failed")]
    ChecksFailed,
    /// `status`: not enrolled (already printed).
    #[error("this station is not enrolled")]
    NotEnrolled,
}

impl CommandError {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Run(RunError::Revoked) => EXIT_REVOKED,
            Self::Run(RunError::Credentials(CredentialsError::NotEnrolled { .. }))
            | Self::Credentials(CredentialsError::NotEnrolled { .. })
            | Self::NotEnrolled => EXIT_NOT_ENROLLED,
            Self::Config(_)
            | Self::Tls(_)
            | Self::Credentials(_)
            | Self::Run(RunError::Tls(_) | RunError::Credentials(_))
            | Self::Render(
                RenderError::ZeroReceiver
                | RenderError::UnknownReceiver(_)
                | RenderError::ProfileNotInBuild { .. }
                | RenderError::Store(StoreError::Invalid { .. }),
            )
            | Self::NotApplicable(_)
            | Self::Enroll(
                EnrollError::StaticCredentials
                | EnrollError::NoEndpoints
                | EnrollError::AlreadyEnrolled { .. },
            ) => EXIT_CONFIG,
            Self::Enroll(
                EnrollError::Token(_) | EnrollError::MalformedToken | EnrollError::NoToken,
            ) => EXIT_TOKEN,
            Self::Run(_)
            | Self::Signals(_)
            | Self::Enroll(_)
            | Self::Render(_)
            | Self::State(_)
            | Self::Output(_)
            | Self::ChecksFailed => EXIT_FAILURE,
        }
    }
}

/// How the receivers of `config` run.
pub fn driver_for(config: &BackendConfig) -> Driver {
    match config {
        BackendConfig::Bundled => Driver::bundled(),
        BackendConfig::External(k) => Driver {
            backends: Arc::new(Ka9qBackends {
                groups: Some((k.status_group, k.data_group)),
            }),
            units: Arc::new(ExternalUnits),
        },
        BackendConfig::Fake => Driver {
            backends: Arc::new(FakeBackends::default()),
            units: Arc::new(FakeUnits::default()),
        },
    }
}

/// `check-config`: loads the config, the credentials and the applied
/// receivers; returns a one-line summary. A station that is not enrolled
/// yet has a valid config.
pub fn check_config(arg: &ConfigArg) -> Result<String, CommandError> {
    let config = arg.load()?;
    if let Some(w) = config.key_permission_warning() {
        warn!("{w}");
    }
    let station = match credentials::load(&config) {
        Ok(c) => format!(
            "station {} -> {} ({} credentials, {} trust anchor(s))",
            c.tls.station_id,
            c.gateway.url,
            credentials::source(&config).kind(),
            c.tls.trust_anchors
        ),
        Err(CredentialsError::NotEnrolled { .. }) => {
            "not enrolled yet (run `stationd enroll`)".to_owned()
        }
        Err(e) => return Err(e.into()),
    };
    let receivers = crate::receivers::StoredSet::load(&config.receivers_path())
        .map_err(|e| CommandError::Render(RenderError::Store(e)))?;
    Ok(format!(
        "config OK ({} build): {station}, backend {}, {} receiver(s) applied in {}",
        config.flavour.name(),
        config.backend,
        receivers.receivers.len(),
        config.receivers_path().display(),
    ))
}

/// `run`: starts the daemon and serves until `stop` resolves (the binary
/// passes SIGTERM/SIGINT), then shuts down gracefully. Keeps the runtime
/// status file up to date.
pub async fn run(
    arg: &ConfigArg,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), CommandError> {
    let config = arg.load()?;
    if let Some(w) = config.key_permission_warning() {
        warn!("{w}");
    }
    let driver = driver_for(&config.backend);
    let mut handle = crate::daemon::spawn(&config, driver, RunOptions::default())?;
    // Development builds keep status.json in the (private) state directory;
    // packaged ones in /run/coaxhub/stationd, which systemd creates.
    let _ = if config.runtime_dir == config.state_dir {
        crate::state::ensure_private_dir(&config.runtime_dir)
    } else {
        std::fs::create_dir_all(&config.runtime_dir).map_err(|source| StateError {
            path: config.runtime_dir.clone(),
            source,
        })
    };
    let status = tokio::spawn(local_status::keep_written(
        config.status_path(),
        handle.station_id().to_string(),
        handle.state(),
        handle.observed(),
        handle.last_error(),
        handle.receivers(),
    ));
    let result = tokio::select! {
        result = handle.join() => result,
        () = stop => {
            info!("stop requested");
            handle.shutdown();
            handle.join().await
        }
    };
    // The writer records the final state, then ends.
    drop(handle);
    if tokio::time::timeout(Duration::from_secs(2), status)
        .await
        .is_err()
    {
        warn!("the runtime status file may be stale");
    }
    result.map_err(CommandError::from)
}

/// `enroll`: reads the token (argument, hidden prompt or stdin), enrolls,
/// and returns what to print.
pub async fn enroll(
    arg: &ConfigArg,
    token: Option<String>,
    force: bool,
) -> Result<String, CommandError> {
    let config = arg.load()?;
    enroll::preflight(&config, force)?;
    let token = enroll::read_token(token)?;
    let e = enroll::enroll(
        &config,
        token,
        EnrollOptions {
            force,
            ..EnrollOptions::default()
        },
    )
    .await?;
    let left = e.validity.not_after_unix_s - unix_now();
    Ok(format!(
        "enrolled: station {}\nhome hub {} ({})\ncertificate expires {} (in {})\n\
         credentials in {}\nstart the service: systemctl enable --now stationd",
        e.station_id,
        e.assignment.hub_id,
        e.assignment.gateway_url,
        format_utc(e.validity.not_after_unix_s),
        format_duration(left),
        config.state_dir.display(),
    ))
}

/// `status`: the report as text or JSON, and whether the station is
/// enrolled.
pub fn status(arg: &ConfigArg, json: bool) -> Result<(String, bool), CommandError> {
    let config = arg.load()?;
    let report = local_status::report(&config, unix_now());
    let text = if json {
        serde_json::to_string_pretty(&report)
            .map_err(|e| CommandError::Output(std::io::Error::other(e)))?
    } else {
        report.render()
    };
    Ok((text, report.enrolled))
}

/// `doctor`: every check, and whether one failed.
pub async fn doctor(arg: &ConfigArg) -> (Vec<doctor::Check>, bool) {
    let checks = doctor::run(arg, &doctor::Environment::default()).await;
    let failed = doctor::failed(&checks);
    (checks, failed)
}

/// `unenroll`: deletes the credentials; returns what to print.
pub fn unenroll(arg: &ConfigArg) -> Result<String, CommandError> {
    let config = arg.load()?;
    if let CredentialsMode::Static(_) = config.credentials {
        return Err(CommandError::NotApplicable(
            "stationd.toml has static [credentials]; there is nothing to unenroll",
        ));
    }
    let dir = StateDir::new(&config.state_dir);
    let removed = dir.remove_credentials()?;
    Ok(if removed.is_empty() {
        format!(
            "not enrolled: no credentials in {}",
            config.state_dir.display()
        )
    } else {
        format!(
            "removed {} from {}; stop the service (systemctl stop stationd) and enroll again to reconnect",
            removed.join(", "),
            config.state_dir.display()
        )
    })
}

/// `backend-config`: renders receiver `receiver`'s radiod config to `out`
/// (default: where its unit reads it; `-`: returned for standard output).
pub fn backend_config(
    arg: &ConfigArg,
    receiver: u32,
    out: Option<&Path>,
    templates: Option<&Path>,
) -> Result<Option<String>, CommandError> {
    let config = arg.load()?;
    let rx = backend_config::find(&config.receivers_path(), receiver, config.flavour.packaged)?;
    let dir = templates.map_or_else(|| config.flavour.templates_dir(), Path::to_path_buf);
    let out = out.map_or_else(|| backend_config::default_out(receiver), Path::to_path_buf);
    if out == Path::new("-") {
        return Ok(Some(backend_config::render_receiver(&dir, &rx)?));
    }
    backend_config::write(&dir, &rx, &out)?;
    info!(receiver_id = receiver, profile = %rx.profile, slot = rx.slot, out = %out.display(), "radiod config written");
    Ok(None)
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

/// Runs `command` and returns the exit code; output goes to stdout, errors
/// to the log (stderr).
pub fn main(command: Command, runtime: &tokio::runtime::Runtime) -> ExitCode {
    let print = |text: &str| {
        println!("{text}");
    };
    let result = match command {
        #[cfg(not(feature = "packaged"))]
        Command::CheckConfig { config } => check_config(&config).map(|s| print(&s)),
        Command::Run { config } => runtime.block_on(async {
            let stop = termination_signal()?;
            run(&config, stop).await
        }),
        Command::Enroll {
            config,
            token,
            force,
        } => runtime
            .block_on(enroll(&config, token, force))
            .map(|s| print(&s)),
        Command::Status { config, json } => status(&config, json).and_then(|(text, enrolled)| {
            print(text.trim_end());
            if enrolled {
                Ok(())
            } else {
                Err(CommandError::NotEnrolled)
            }
        }),
        Command::Doctor { config } => {
            let (checks, failed) = runtime.block_on(doctor(&config));
            for c in &checks {
                print(&c.to_string());
            }
            if failed {
                Err(CommandError::ChecksFailed)
            } else {
                Ok(())
            }
        }
        Command::Unenroll { config } => unenroll(&config).map(|s| print(&s)),
        Command::BackendConfig {
            config,
            receiver,
            out,
            templates,
        } => backend_config(&config, receiver, out.as_deref(), templates.as_deref()).map(|text| {
            if let Some(t) = text {
                print!("{t}");
            }
        }),
    };
    exit_code(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[cfg(not(feature = "packaged"))]
    #[test]
    fn parses_subcommands_and_defaults() {
        use crate::config::DEFAULT_CONFIG_PATH;
        let c = Cli::try_parse_from(["stationd", "run", "--config", "/tmp/s.toml"]).expect("run");
        assert_eq!(
            c.command,
            Command::Run {
                config: ConfigArg::at("/tmp/s.toml")
            }
        );
        let c = Cli::try_parse_from(["stationd", "run"]).expect("run");
        assert_eq!(
            c.command,
            Command::Run {
                config: ConfigArg::at(DEFAULT_CONFIG_PATH)
            }
        );
        let c = Cli::try_parse_from(["stationd", "check-config", "-c", "x.toml"]).expect("check");
        assert_eq!(
            c.command,
            Command::CheckConfig {
                config: ConfigArg::at("x.toml")
            }
        );
        let c = Cli::try_parse_from(["stationd", "enroll", "--token", "t", "--force"]).expect("e");
        assert_eq!(
            c.command,
            Command::Enroll {
                config: ConfigArg::at(DEFAULT_CONFIG_PATH),
                token: Some("t".into()),
                force: true
            }
        );
        let c = Cli::try_parse_from(["stationd", "enroll"]).expect("prompt");
        assert!(matches!(
            c.command,
            Command::Enroll {
                token: None,
                force: false,
                ..
            }
        ));
        let c = Cli::try_parse_from(["stationd", "status", "--json"]).expect("status");
        assert!(matches!(c.command, Command::Status { json: true, .. }));
        let c =
            Cli::try_parse_from(["stationd", "backend-config", "--receiver", "11"]).expect("bc");
        assert_eq!(
            c.command,
            Command::BackendConfig {
                config: ConfigArg::at(DEFAULT_CONFIG_PATH),
                receiver: 11,
                out: None,
                templates: None
            }
        );
        assert!(Cli::try_parse_from(["stationd", "backend-config"]).is_err());
        assert!(Cli::try_parse_from(["stationd", "backend-config", "--receiver", "x"]).is_err());
        for sub in ["doctor", "unenroll"] {
            assert!(Cli::try_parse_from(["stationd", sub]).is_ok(), "{sub}");
        }
        let e = Cli::try_parse_from(["stationd", "--version"]).expect_err("version exits");
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(e.to_string().contains(env!("CARGO_PKG_VERSION")));
        let e = Cli::try_parse_from(["stationd"]).expect_err("subcommand required");
        assert_eq!(e.exit_code(), i32::from(EXIT_CONFIG));
        assert!(Cli::try_parse_from(["stationd", "enroll", "--tokn", "x"]).is_err());
        assert_eq!(ConfigArg::at("/x.toml").describe(), "/x.toml");
    }

    #[test]
    fn exit_codes() {
        let revoked = CommandError::Run(RunError::Revoked);
        assert_eq!(revoked.exit_code(), EXIT_REVOKED);
        let task = CommandError::Run(RunError::Task("x".into()));
        assert_eq!(task.exit_code(), EXIT_FAILURE);
        let tls = CommandError::Tls(tls::TlsError::ServerName("x".into()));
        assert_eq!(tls.exit_code(), EXIT_CONFIG);
        assert_eq!(exit_code(Ok(())), ExitCode::from(EXIT_OK));
        assert_eq!(exit_code(Err(revoked)), ExitCode::from(EXIT_REVOKED));
        let sig = CommandError::Signals(std::io::Error::other("x"));
        assert_eq!(sig.exit_code(), EXIT_FAILURE);
        let not = CredentialsError::NotEnrolled {
            dir: "/x".into(),
            missing: vec!["key.pem"],
        };
        assert_eq!(
            CommandError::Run(RunError::Credentials(not)).exit_code(),
            EXIT_NOT_ENROLLED
        );
        assert_eq!(CommandError::NotEnrolled.exit_code(), EXIT_NOT_ENROLLED);
        assert_eq!(
            CommandError::Enroll(EnrollError::NoEndpoints).exit_code(),
            EXIT_CONFIG
        );
        for token in [
            EnrollError::MalformedToken,
            EnrollError::NoToken,
            EnrollError::Token(enroll::TokenProblem::Expired),
        ] {
            assert_eq!(CommandError::Enroll(token).exit_code(), EXIT_TOKEN);
        }
        assert_eq!(
            CommandError::Enroll(EnrollError::Timeout {
                endpoint: "x".into()
            })
            .exit_code(),
            EXIT_FAILURE
        );
        for refused in [
            RenderError::ZeroReceiver,
            RenderError::UnknownReceiver(9),
            RenderError::ProfileNotInBuild {
                receiver_id: 9,
                profile: "siggen",
            },
        ] {
            assert_eq!(CommandError::Render(refused).exit_code(), EXIT_CONFIG);
        }
        assert_eq!(
            CommandError::Render(RenderError::Template {
                path: "/x".into(),
                source: std::io::Error::other("x")
            })
            .exit_code(),
            EXIT_FAILURE
        );
        assert_eq!(CommandError::ChecksFailed.exit_code(), EXIT_FAILURE);
        assert_eq!(CommandError::NotApplicable("x").exit_code(), EXIT_CONFIG);
        assert_eq!(
            CommandError::Output(std::io::Error::other("x")).exit_code(),
            EXIT_FAILURE
        );
    }

    #[cfg(not(feature = "packaged"))]
    #[test]
    fn a_missing_config_is_a_config_error() {
        let missing =
            check_config(&ConfigArg::at("/nonexistent/stationd.toml")).expect_err("missing");
        assert_eq!(missing.exit_code(), EXIT_CONFIG);
    }

    #[tokio::test]
    async fn drivers_are_built_from_the_config() {
        let fake = driver_for(&BackendConfig::Fake);
        assert!(fake.backends.info().version.starts_with("fake-"));
        let bundled = driver_for(&BackendConfig::Bundled);
        assert!(bundled.backends.info().version.starts_with("ka9q-radio"));
        let external = driver_for(&BackendConfig::External(crate::config::Ka9qSettings {
            status_group: "127.0.0.1:9".parse().expect("addr"),
            data_group: Some("127.0.0.1:0".parse().expect("addr")),
        }));
        assert!(external.backends.can_measure());
        assert!(matches!(
            external.units.state(1).await,
            crate::receivers::UnitState::Active { .. }
        ));
    }
}
