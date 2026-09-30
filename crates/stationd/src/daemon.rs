// SPDX-License-Identifier: GPL-3.0-only
//! The reconnect loop (station-protocol §2.2) and the in-process daemon API.
//!
//! [`spawn`] starts the daemon on the current Tokio runtime and returns a
//! [`DaemonHandle`]: tests (and `main`) observe the [`ConnectionState`] via a
//! `watch`, request a graceful shutdown, and join for the exit reason.
//!
//! Reconnection follows `station_proto::backoff` exactly: every session end
//! (connect failure, lost stream, `Goodbye`) goes through
//! [`Backoff::on_disconnected`], which also handles `retry_after_ms`,
//! `DISABLED` (15 min), `UNSUPPORTED_VERSION` (6 h) and `REVOKED` (stop for
//! good: [`run`] returns [`RunError::Revoked`]).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use station_backend::Backend;
use station_proto::backoff::{Backoff, Decision, Policy};
use station_proto::{limits, v1};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::{Config, Gateway, LocalPolicy};
use crate::dispatch::truncate_utf8;
use crate::outbound::{DropCounters, QueueLimits};
use crate::session::{self, End};
use crate::tls::{self, Connector, TlsError};

use v1::goodbye::Reason;

/// Timing and policy knobs. [`RunOptions::default`] is the production
/// behaviour; tests shorten the intervals.
pub struct RunOptions {
    /// Reconnection policy (§2.2).
    pub backoff: Policy,
    /// Random source for the backoff jitter.
    pub random: Box<dyn FnMut() -> u64 + Send>,
    /// TCP + TLS connect timeout.
    pub connect_timeout: Duration,
    /// How long to wait for `Welcome` after opening the stream.
    pub welcome_timeout: Duration,
    /// HTTP/2 keepalive ping interval (§2.1: 20 s).
    pub keepalive_interval: Duration,
    /// HTTP/2 keepalive ping timeout (§2.1: 10 s).
    pub keepalive_timeout: Duration,
    /// On shutdown, how long to wait for the queue to flush and the hub to
    /// close the stream after `Event{SHUTTING_DOWN}`.
    pub shutdown_grace: Duration,
    /// Outbound queue bounds (§6.3).
    pub queue: QueueLimits,
    /// The `Hello.boot_id`; random when `None` (once per daemon).
    pub boot_id: Option<u64>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            backoff: Policy::default(),
            random: Box::new(rand::random::<u64>),
            connect_timeout: Duration::from_secs(10),
            welcome_timeout: limits::HELLO_TIMEOUT + Duration::from_secs(5),
            keepalive_interval: limits::HTTP2_KEEPALIVE_INTERVAL,
            keepalive_timeout: limits::HTTP2_KEEPALIVE_TIMEOUT,
            shutdown_grace: Duration::from_secs(2),
            queue: QueueLimits::default(),
            boot_id: None,
        }
    }
}

impl fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunOptions")
            .field("backoff", &self.backoff)
            .field("connect_timeout", &self.connect_timeout)
            .field("welcome_timeout", &self.welcome_timeout)
            .field("keepalive_interval", &self.keepalive_interval)
            .field("keepalive_timeout", &self.keepalive_timeout)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("queue", &self.queue)
            .field("boot_id", &self.boot_id)
            .finish_non_exhaustive()
    }
}

/// Why the daemon stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Graceful shutdown was requested.
    Shutdown,
    /// The hub sent `Goodbye{REVOKED}`: re-enrollment is required.
    Revoked,
}

/// The tunnel's state, observable through [`DaemonHandle::state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Dialing the gateway, or waiting for `Welcome`.
    Connecting {
        /// Attempt number since start, from 1.
        attempt: u64,
    },
    /// Session established (`Welcome` received).
    Connected {
        /// The hub's `Welcome.connection_id` (truncated to 64 bytes).
        connection_id: String,
        /// The negotiated protocol version.
        version: v1::ProtocolVersion,
    },
    /// Waiting before the next attempt.
    Backoff {
        /// How long.
        delay: Duration,
    },
    /// Not running any more.
    Stopped(StopReason),
}

/// A fatal daemon error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// The TLS credentials could not be loaded.
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// The hub revoked this station's credential (`Goodbye{REVOKED}`).
    #[error("the hub revoked this station's certificate; enroll the station again")]
    Revoked,
    /// The daemon task panicked or was aborted.
    #[error("daemon task failed: {0}")]
    Task(String),
    /// [`DaemonHandle::join`] was called again after it returned.
    #[error("daemon already joined")]
    AlreadyJoined,
}

/// Everything a session needs, shared across connections.
pub(crate) struct Ctx {
    pub gateway: Gateway,
    pub connector: Connector,
    pub policy: Arc<LocalPolicy>,
    pub backend: Arc<dyn Backend>,
    pub boot_id: u64,
    pub started: tokio::time::Instant,
    pub drops: Arc<DropCounters>,
    pub connect_timeout: Duration,
    pub welcome_timeout: Duration,
    pub keepalive_interval: Duration,
    pub keepalive_timeout: Duration,
    pub shutdown_grace: Duration,
    pub queue: QueueLimits,
}

/// A running daemon.
#[derive(Debug)]
pub struct DaemonHandle {
    state: watch::Receiver<ConnectionState>,
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<(), RunError>>>,
    station_id: Uuid,
    boot_id: u64,
}

impl DaemonHandle {
    /// A receiver of the connection state.
    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    /// The current connection state.
    pub fn current_state(&self) -> ConnectionState {
        self.state.borrow().clone()
    }

    /// The station ID from the client certificate.
    pub fn station_id(&self) -> Uuid {
        self.station_id
    }

    /// This daemon's `Hello.boot_id`.
    pub fn boot_id(&self) -> u64 {
        self.boot_id
    }

    /// Requests a graceful shutdown (idempotent): `Event{SHUTTING_DOWN}` if
    /// connected, close the stream, stop.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Whether the daemon task has ended.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits for the daemon to stop. Cancel-safe; after it returned once,
    /// further calls return [`RunError::AlreadyJoined`].
    pub async fn join(&mut self) -> Result<(), RunError> {
        let task = self.task.as_mut().ok_or(RunError::AlreadyJoined)?;
        let result = task.await;
        self.task = None;
        result.map_err(|e| RunError::Task(e.to_string()))?
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Loads the TLS credentials and starts the daemon on the current runtime.
pub fn spawn(
    config: &Config,
    backend: Arc<dyn Backend>,
    options: RunOptions,
) -> Result<DaemonHandle, RunError> {
    let tls = tls::load(config)?;
    let station_id = tls.station_id;
    let shutdown = CancellationToken::new();
    let (state_tx, state) = watch::channel(ConnectionState::Connecting { attempt: 0 });
    let boot_id = options.boot_id.unwrap_or_else(rand::random::<u64>);
    let options = RunOptions {
        boot_id: Some(boot_id),
        ..options
    };
    let ctx = context(config, &tls, backend, &options)?;
    info!(
        station_id = %station_id,
        gateway = %config.gateway.url,
        trust_anchors = tls.trust_anchors,
        boot_id,
        "stationd starting"
    );
    let task = tokio::spawn(run_loop(ctx, options, shutdown.clone(), state_tx));
    Ok(DaemonHandle {
        state,
        shutdown,
        task: Some(task),
        station_id,
        boot_id,
    })
}

/// Runs the daemon until `shutdown` is cancelled (`Ok`) or the credential is
/// revoked. State changes are published on `state`.
pub async fn run(
    config: &Config,
    backend: Arc<dyn Backend>,
    options: RunOptions,
    shutdown: CancellationToken,
    state: watch::Sender<ConnectionState>,
) -> Result<(), RunError> {
    let tls = tls::load(config)?;
    let ctx = context(config, &tls, backend, &options)?;
    run_loop(ctx, options, shutdown, state).await
}

fn context(
    config: &Config,
    tls: &tls::ClientTls,
    backend: Arc<dyn Backend>,
    options: &RunOptions,
) -> Result<Arc<Ctx>, RunError> {
    Ok(Arc::new(Ctx {
        gateway: config.gateway.clone(),
        connector: Connector::new(Arc::clone(&tls.config), &config.gateway)?,
        policy: Arc::new(config.policy.clone()),
        backend,
        boot_id: options.boot_id.unwrap_or_else(rand::random::<u64>),
        started: tokio::time::Instant::now(),
        drops: Arc::default(),
        connect_timeout: options.connect_timeout,
        welcome_timeout: options.welcome_timeout,
        keepalive_interval: options.keepalive_interval,
        keepalive_timeout: options.keepalive_timeout,
        shutdown_grace: options.shutdown_grace,
        queue: options.queue,
    }))
}

fn std_now() -> std::time::Instant {
    // Tokio's clock, so a paused test clock also drives the backoff.
    tokio::time::Instant::now().into_std()
}

fn log_goodbye(g: &v1::Goodbye) {
    let message = truncate_utf8(&g.message, limits::MAX_DETAIL_BYTES);
    let retry_after_ms = g.retry_after_ms;
    match Reason::try_from(g.reason) {
        Ok(Reason::Shutdown | Reason::Draining) => info!(
            reason = g.reason().as_str_name(),
            retry_after_ms,
            hub_message = ?message,
            "hub is restarting or draining; will reconnect"
        ),
        Ok(Reason::UnsupportedVersion) => error!(
            hub_message = ?message,
            "THE HUB DOES NOT SUPPORT THIS STATIOND VERSION: update stationd \
             (coaxhub-stationd package); retrying every 6 hours"
        ),
        Ok(Reason::Disabled) => warn!(
            hub_message = ?message,
            "this station is disabled by its owner or a moderator; retrying every 15 minutes"
        ),
        Ok(Reason::Revoked) => error!(
            hub_message = ?message,
            "the hub revoked this station's certificate; stationd stops. \
             Enroll the station again to reconnect"
        ),
        Ok(Reason::ProtocolViolation) => error!(
            hub_message = ?message,
            "the hub reported a protocol violation by this station (a stationd or backend bug); reconnecting"
        ),
        Ok(reason) => warn!(
            reason = reason.as_str_name(),
            retry_after_ms,
            hub_message = ?message,
            "hub ended the session"
        ),
        Err(_) => warn!(
            reason = g.reason,
            retry_after_ms,
            hub_message = ?message,
            "hub ended the session with an unknown reason"
        ),
    }
}

async fn run_loop(
    ctx: Arc<Ctx>,
    mut options: RunOptions,
    shutdown: CancellationToken,
    state: watch::Sender<ConnectionState>,
) -> Result<(), RunError> {
    let mut backoff = Backoff::new(options.backoff);
    let mut attempt: u64 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        state.send_replace(ConnectionState::Connecting { attempt });
        let outcome = session::run(&ctx, &shutdown, &state).await;
        if let Some(at) = outcome.established_at {
            backoff.on_connected(at);
        }
        let goodbye = match outcome.end {
            End::Shutdown => {
                info!("stationd stopped");
                state.send_replace(ConnectionState::Stopped(StopReason::Shutdown));
                return Ok(());
            }
            End::Goodbye(g) => {
                log_goodbye(&g);
                Some(g)
            }
            End::Lost(reason) => {
                warn!(attempt, reason = %reason, "connection to the hub lost or failed");
                None
            }
        };
        match backoff.on_disconnected(goodbye.as_ref(), std_now(), &mut options.random) {
            Decision::Stop => {
                state.send_replace(ConnectionState::Stopped(StopReason::Revoked));
                return Err(RunError::Revoked);
            }
            Decision::Reconnect {
                delay,
                refresh_assignment,
            } => {
                if refresh_assignment {
                    warn!(
                        "the hub asked to refresh the assignment; StationControl.GetAssignment \
                         is not implemented yet, reconnecting to the configured gateway_url"
                    );
                }
                info!(
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "reconnecting after backoff"
                );
                state.send_replace(ConnectionState::Backoff { delay });
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = shutdown.cancelled() => {
                        info!("stationd stopped");
                        state.send_replace(ConnectionState::Stopped(StopReason::Shutdown));
                        return Ok(());
                    }
                }
            }
        }
    }
}
