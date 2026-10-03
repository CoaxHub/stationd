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
//! `DISABLED` (5 min) and `UNSUPPORTED_VERSION` (6 h).
//!
//! Enrolled stations with a `station-api` endpoint also talk to the control
//! plane ([`crate::station_api`]):
//!
//! - **Assignment:** `GetAssignment` before the first connection and after
//!   `Goodbye{refresh_assignment}` or `REASSIGNED`, stored in
//!   `assignment.json`; if `station-api` cannot be reached, the stored
//!   assignment is used.
//! - **Renewal:** a task ([`crate::renew`]) renews the certificate at two
//!   thirds of its lifetime; the tunnel then reconnects at once with it.
//! - **Revocation:** `Goodbye{REVOKED}`, or `revoked` from `station-api`,
//!   stops the daemon ([`RunError::Revoked`], exit code 3), **unless** the
//!   state directory now holds a different, unexpired certificate than the
//!   refused one (a renewal or a re-enrollment just landed): then it
//!   reconnects with that one.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use station_backend::Backend;
use station_proto::backoff::{Backoff, Decision, Policy};
use station_proto::{limits, v1};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::{Config, CredentialsMode, Endpoint, LocalPolicy};
use crate::credentials::{self, CredentialSource, Credentials, CredentialsError};
use crate::dispatch::truncate_utf8;
use crate::measure::{FixedLoad, LoadSource, MeasureClock, ProcLoad, SystemClock};
use crate::outbound::{DropCounters, QueueLimits};
use crate::renew::{self, ApiRenewal, RenewalOptions, RenewalStatus, Signal, UnixClock};
use crate::session::{self, End};
use crate::state::StateDir;
use crate::station_api::{ControlError, StationApi};
use crate::tls::{Connector, TlsError};

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
    /// Certificate renewal timing (and the wall clock the daemon uses).
    pub renewal: RenewalOptions,
    /// Timeout of `StationControl.GetAssignment` (connect and call).
    pub assignment_timeout: Duration,
    /// Timeout of `StationControl.RenewCertificate` (connect and call).
    pub renew_timeout: Duration,
    /// The wall clock the measurement schedule follows (the measurement design §4.1): the
    /// system clock. Tests run it faster ([`crate::measure::ScaledClock`]);
    /// `stationd.toml` can't change it.
    pub measure_clock: Arc<dyn MeasureClock>,
    /// Where measuring's pause rule reads the host's load (the measurement design §4.1):
    /// `/proc/loadavg`. Tests use a fixed load ([`crate::measure::FixedLoad`])
    /// so that measuring doesn't depend on the machine running them;
    /// `[measurements] ignore_host_load` (development builds) does the same
    /// for the binary.
    pub measure_load: Arc<dyn LoadSource>,
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
            renewal: RenewalOptions::default(),
            assignment_timeout: crate::station_api::ASSIGNMENT_TIMEOUT,
            renew_timeout: crate::station_api::RENEW_TIMEOUT,
            measure_clock: Arc::new(SystemClock),
            measure_load: Arc::new(ProcLoad),
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
            .field("renewal", &self.renewal)
            .field("assignment_timeout", &self.assignment_timeout)
            .field("renew_timeout", &self.renew_timeout)
            .field("measure_clock", &self.measure_clock)
            .field("measure_load", &self.measure_load)
            .finish_non_exhaustive()
    }
}

/// Why the daemon stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Graceful shutdown was requested.
    Shutdown,
    /// The certificate was revoked (`Goodbye{REVOKED}`, or `revoked` from
    /// `station-api`): re-enrollment is required.
    Revoked,
}

/// What the daemon learned beyond the connection state, observable through
/// [`DaemonHandle::observed`] (and written to `status.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// The gateway being dialed (it changes with the assignment).
    pub gateway_url: String,
    /// The home hub, when known (enrolled stations).
    pub hub_id: Option<String>,
    /// The serial of the certificate in use, hex.
    pub serial: String,
    /// Hub time minus local time when the last `Welcome` arrived,
    /// milliseconds (round-trip time ignored); `None` until a hub reported
    /// its time.
    pub clock_skew_ms: Option<i64>,
    /// Certificate renewal.
    pub renewal: RenewalStatus,
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
    /// The credentials could not be loaded (not enrolled, unreadable
    /// state).
    #[error(transparent)]
    Credentials(CredentialsError),
    /// The station's certificate was revoked (`Goodbye{REVOKED}`, or
    /// `revoked` from `station-api`).
    #[error("this station's certificate was revoked; enroll the station again")]
    Revoked,
    /// The daemon task panicked or was aborted.
    #[error("daemon task failed: {0}")]
    Task(String),
    /// [`DaemonHandle::join`] was called again after it returned.
    #[error("daemon already joined")]
    AlreadyJoined,
}

impl From<CredentialsError> for RunError {
    fn from(e: CredentialsError) -> Self {
        match e {
            CredentialsError::Tls(t) => Self::Tls(t),
            other => Self::Credentials(other),
        }
    }
}

/// Where and how one connection attempt dials.
#[derive(Debug, Clone)]
pub(crate) struct Link {
    pub gateway: Endpoint,
    pub connector: Connector,
    pub hub_id: Option<String>,
    /// The serial of the certificate this link presents.
    pub serial: String,
}

impl Link {
    fn new(credentials: &Credentials) -> Result<Self, TlsError> {
        Ok(Self {
            gateway: credentials.gateway.clone(),
            connector: Connector::new(Arc::clone(&credentials.tls.config), &credentials.gateway)?,
            hub_id: credentials.hub_id.clone(),
            serial: credentials.tls.serial.clone(),
        })
    }

    fn publish(&self, observed: &watch::Sender<Observed>) {
        observed.send_modify(|o| {
            o.gateway_url.clone_from(&self.gateway.url);
            o.hub_id.clone_from(&self.hub_id);
            o.serial.clone_from(&self.serial);
        });
    }
}

/// Everything a session needs, shared across connections.
pub(crate) struct Ctx {
    pub credentials: Arc<dyn CredentialSource>,
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
    /// `StationControl`, for enrolled stations with a `station-api`
    /// endpoint.
    pub control: Option<Arc<StationApi>>,
    /// What the daemon observed.
    pub observed: watch::Sender<Observed>,
    pub measure_clock: Arc<dyn MeasureClock>,
    pub measure_load: Arc<dyn LoadSource>,
    /// The wall clock, Unix seconds.
    pub clock: UnixClock,
}

/// A running daemon.
#[derive(Debug)]
pub struct DaemonHandle {
    state: watch::Receiver<ConnectionState>,
    last_error: watch::Receiver<Option<String>>,
    observed: watch::Receiver<Observed>,
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<(), RunError>>>,
    station_id: Uuid,
    gateway_url: String,
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

    /// Why the last connection attempt or session failed (`None` after a
    /// session was established and until the next failure).
    pub fn last_error(&self) -> watch::Receiver<Option<String>> {
        self.last_error.clone()
    }

    /// A receiver of what the daemon observed (gateway, clock skew,
    /// renewal).
    pub fn observed(&self) -> watch::Receiver<Observed> {
        self.observed.clone()
    }

    /// The station ID from the client certificate.
    pub fn station_id(&self) -> Uuid {
        self.station_id
    }

    /// The gateway the daemon started with (see [`Self::observed`] for the
    /// current one).
    pub fn gateway_url(&self) -> &str {
        &self.gateway_url
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

/// Loads the credentials `config` names and starts the daemon on the
/// current runtime.
pub fn spawn(
    config: &Config,
    backend: Arc<dyn Backend>,
    options: RunOptions,
) -> Result<DaemonHandle, RunError> {
    spawn_with(credentials::source(config), config, backend, options)
}

/// Starts the daemon with credentials from `source` (re-read before every
/// connect attempt). Fails if they cannot be loaded now.
pub fn spawn_with(
    source: Arc<dyn CredentialSource>,
    config: &Config,
    backend: Arc<dyn Backend>,
    options: RunOptions,
) -> Result<DaemonHandle, RunError> {
    finish_interrupted_swap(config);
    let first = source.load()?;
    let link = Link::new(&first)?;
    let station_id = first.tls.station_id;
    let shutdown = CancellationToken::new();
    let (state_tx, state) = watch::channel(ConnectionState::Connecting { attempt: 0 });
    let (error_tx, last_error) = watch::channel(None);
    let boot_id = options.boot_id.unwrap_or_else(rand::random::<u64>);
    let options = RunOptions {
        boot_id: Some(boot_id),
        ..options
    };
    info!(
        station_id = %station_id,
        gateway = %first.gateway.url,
        hub = first.hub_id.as_deref().unwrap_or("-"),
        credentials = source.kind(),
        trust_anchors = first.tls.trust_anchors,
        serial = %first.tls.serial,
        boot_id,
        "stationd starting"
    );
    let (ctx, options) = context(source, config, backend, options);
    link.publish(&ctx.observed);
    let observed = ctx.observed.subscribe();
    let task = tokio::spawn(run_loop(
        ctx,
        link,
        options,
        shutdown.clone(),
        state_tx,
        error_tx,
    ));
    Ok(DaemonHandle {
        state,
        last_error,
        observed,
        shutdown,
        task: Some(task),
        station_id,
        gateway_url: first.gateway.url,
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
    finish_interrupted_swap(config);
    let source = credentials::source(config);
    let link = Link::new(&source.load()?)?;
    let (ctx, options) = context(source, config, backend, options);
    link.publish(&ctx.observed);
    let (error_tx, _) = watch::channel(None);
    run_loop(ctx, link, options, shutdown, state, error_tx).await
}

/// Completes a credential swap a crash interrupted (enrolled stations).
fn finish_interrupted_swap(config: &Config) {
    if let CredentialsMode::Enrolled = config.credentials {
        match StateDir::new(&config.state_dir).complete_swap() {
            Ok(true) => info!("completed a certificate swap interrupted by a restart"),
            Ok(false) => {}
            Err(e) => warn!(error = %e, "cannot complete an interrupted certificate swap"),
        }
    }
}

/// The options the reconnect loop keeps (the renewal task takes the rest).
pub(crate) struct LoopOptions {
    backoff: Policy,
    random: Box<dyn FnMut() -> u64 + Send>,
    renewal: Option<RenewalOptions>,
}

fn context(
    credentials: Arc<dyn CredentialSource>,
    config: &Config,
    backend: Arc<dyn Backend>,
    options: RunOptions,
) -> (Arc<Ctx>, LoopOptions) {
    let control = StationApi::from_config(config).map(|mut api| {
        api.assignment_timeout = options.assignment_timeout;
        api.renew_timeout = options.renew_timeout;
        Arc::new(api)
    });
    if control.is_none() && matches!(config.credentials, CredentialsMode::Enrolled) {
        info!("no station-api endpoint: assignment refresh and certificate renewal are off");
    }
    let measure_load: Arc<dyn LoadSource> = if config.ignore_host_load {
        warn!(
            "[measurements] ignore_host_load: measuring does not pause for the host's load \
             (a development setting)"
        );
        Arc::new(FixedLoad::idle())
    } else {
        Arc::clone(&options.measure_load)
    };
    let (observed, _) = watch::channel(Observed::default());
    let ctx = Arc::new(Ctx {
        credentials,
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
        control,
        observed,
        clock: Arc::clone(&options.renewal.clock),
        measure_clock: Arc::clone(&options.measure_clock),
        measure_load,
    });
    let rest = LoopOptions {
        backoff: options.backoff,
        random: options.random,
        renewal: Some(options.renewal),
    };
    (ctx, rest)
}

/// The link for the next attempt: fresh credentials when they load (a
/// renewed certificate, a new assignment), else the last good ones.
fn refresh(ctx: &Ctx, link: &mut Link) {
    match ctx
        .credentials
        .load()
        .map_err(RunError::from)
        .and_then(|c| {
            let next = Link::new(&c)?;
            Ok((c, next))
        }) {
        Ok((c, next)) => {
            if next.gateway != link.gateway {
                info!(gateway = %c.gateway.url, hub = c.hub_id.as_deref().unwrap_or("-"), "gateway changed");
            }
            if next.serial != link.serial {
                info!(serial = %next.serial, previous = %link.serial, "using a new certificate");
            }
            *link = next;
            link.publish(&ctx.observed);
        }
        Err(e) => warn!(error = %e, "cannot reload the credentials; using the previous ones"),
    }
}

/// The credentials on disk when they hold a different, unexpired
/// certificate than `refused` (a renewal or a re-enrollment landed).
fn newer_than(ctx: &Ctx, refused: &str) -> Option<Credentials> {
    let c = ctx.credentials.load().ok()?;
    (c.tls.serial != refused && c.tls.validity.not_after_unix_s > (ctx.clock)()).then_some(c)
}

/// After `by` refused the certificate `serial` as revoked: `Ok` to
/// reconnect at once with a newer certificate from disk, else the daemon
/// stops (status `revoked`, exit code 3).
fn on_revoked(
    ctx: &Ctx,
    serial: &str,
    by: &str,
    state: &watch::Sender<ConnectionState>,
    last_error: &watch::Sender<Option<String>>,
) -> Result<(), RunError> {
    if let Some(c) = newer_than(ctx, serial) {
        info!(
            refused_serial = %serial,
            serial = %c.tls.serial,
            "{by} refused the previous certificate as revoked; a newer certificate is on disk, \
             reconnecting with it"
        );
        return Ok(());
    }
    error!(
        serial = %serial,
        "{by} revoked this station's certificate; stationd stops. Enroll the station again \
         (stationd enroll --force) to reconnect"
    );
    last_error.send_replace(Some(format!("certificate revoked ({by})")));
    state.send_replace(ConnectionState::Stopped(StopReason::Revoked));
    Err(RunError::Revoked)
}

/// How a `GetAssignment` round ended.
enum Assigned {
    /// Done, or failed and the stored assignment stays.
    Done,
    /// `station-api` refused the certificate with this serial as revoked.
    Revoked(String),
    /// Shutdown was requested meanwhile.
    Shutdown,
}

/// `GetAssignment` (station-protocol §2.2, brief contract C): stores a new
/// home hub; on any failure but `revoked`, keeps the stored one.
async fn refresh_assignment(ctx: &Ctx, shutdown: &CancellationToken) -> Assigned {
    let Some(api) = &ctx.control else {
        return Assigned::Done;
    };
    let result = tokio::select! {
        r = api.refresh_assignment() => r,
        () = shutdown.cancelled() => return Assigned::Shutdown,
    };
    match result {
        Ok(u) if u.changed => {
            info!(
                hub = %u.assignment.hub_id,
                gateway = %u.assignment.gateway_url,
                "new home hub assignment stored"
            );
            Assigned::Done
        }
        Ok(u) => {
            debug!(hub = %u.assignment.hub_id, "home hub assignment unchanged");
            Assigned::Done
        }
        Err(ControlError::Revoked { serial }) => Assigned::Revoked(serial),
        Err(e) => {
            warn!(error = %e, "cannot refresh the home hub assignment; using the stored one");
            Assigned::Done
        }
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_renewal(
    ctx: &Arc<Ctx>,
    options: RenewalOptions,
    shutdown: &CancellationToken,
    signals: mpsc::Sender<Signal>,
) -> Option<AbortOnDrop> {
    let api = ctx.control.as_ref()?;
    let renewal = Arc::new(ApiRenewal {
        api: Arc::clone(api),
        source: Arc::clone(&ctx.credentials),
    });
    let observed = ctx.observed.clone();
    Some(AbortOnDrop(tokio::spawn(renew::run(
        renewal,
        options,
        shutdown.clone(),
        signals,
        move |s: &RenewalStatus| observed.send_modify(|o| o.renewal = s.clone()),
    ))))
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
            "this station is disabled by its owner or a moderator; retrying every 5 minutes"
        ),
        Ok(Reason::Revoked) => warn!(
            hub_message = ?message,
            "the hub refused this station's certificate as revoked"
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
    mut link: Link,
    mut options: LoopOptions,
    shutdown: CancellationToken,
    state: watch::Sender<ConnectionState>,
    last_error: watch::Sender<Option<String>>,
) -> Result<(), RunError> {
    // The sender stays here too, so `recv` never reports a closed channel.
    let (signal_tx, mut signals) = mpsc::channel(4);
    let _renewal = options
        .renewal
        .take()
        .and_then(|r| start_renewal(&ctx, r, &shutdown, signal_tx.clone()));
    let mut backoff = Backoff::new(options.backoff);
    let mut attempt: u64 = 0;
    let mut assign = ctx.control.is_some();
    let stopped = |state: &watch::Sender<ConnectionState>| {
        info!("stationd stopped");
        state.send_replace(ConnectionState::Stopped(StopReason::Shutdown));
        Ok(())
    };
    loop {
        let assign_now = std::mem::take(&mut assign);
        if !before_attempt(
            &ctx,
            assign_now,
            &shutdown,
            &mut signals,
            &state,
            &last_error,
        )
        .await?
        {
            return stopped(&state);
        }
        attempt = attempt.saturating_add(1);
        refresh(&ctx, &mut link);
        state.send_replace(ConnectionState::Connecting { attempt });
        let outcome = session::run(&ctx, &link, &shutdown, &state, &mut signals).await;
        if let Some(at) = outcome.established_at {
            backoff.on_connected(at);
            last_error.send_replace(None);
        }
        let goodbye = match outcome.end {
            End::Shutdown => return stopped(&state),
            End::Renewed => {
                info!("reconnecting with the renewed certificate");
                continue;
            }
            End::Revoked(serial) => {
                on_revoked(&ctx, &serial, "station-api", &state, &last_error)?;
                continue;
            }
            End::Goodbye(g) => {
                log_goodbye(&g);
                if g.reason() == Reason::Revoked {
                    on_revoked(&ctx, &link.serial, "the hub", &state, &last_error)?;
                    continue;
                }
                last_error.send_replace(Some(format!(
                    "hub said goodbye: {}",
                    g.reason().as_str_name()
                )));
                Some(g)
            }
            End::Lost(reason) => {
                warn!(attempt, reason = %reason, "connection to the hub lost or failed");
                last_error.send_replace(Some(reason));
                None
            }
        };
        match backoff.on_disconnected(goodbye.as_ref(), std_now(), &mut options.random) {
            Decision::Stop => {
                // Only REVOKED stops, and it is handled above.
                state.send_replace(ConnectionState::Stopped(StopReason::Revoked));
                return Err(RunError::Revoked);
            }
            Decision::Reconnect {
                delay,
                refresh_assignment,
            } => {
                if refresh_assignment {
                    if ctx.control.is_some() {
                        assign = true;
                    } else {
                        warn!(
                            "the hub asked to refresh the assignment, but there is no station-api \
                             endpoint; reconnecting to the configured gateway"
                        );
                    }
                }
                info!(
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "reconnecting after backoff"
                );
                state.send_replace(ConnectionState::Backoff { delay });
                if !wait_backoff(&ctx, delay, &shutdown, &mut signals, &state, &last_error).await? {
                    return stopped(&state);
                }
            }
        }
    }
}

/// Between sessions: handles signals that arrived meanwhile (a renewal is
/// picked up by the reload that follows; a revocation is handled now), then
/// refreshes the assignment when `assign`. `Ok(false)` on shutdown.
async fn before_attempt(
    ctx: &Ctx,
    assign: bool,
    shutdown: &CancellationToken,
    signals: &mut mpsc::Receiver<Signal>,
    state: &watch::Sender<ConnectionState>,
    last_error: &watch::Sender<Option<String>>,
) -> Result<bool, RunError> {
    while let Ok(signal) = signals.try_recv() {
        if let Signal::Revoked { serial } = signal {
            on_revoked(ctx, &serial, "station-api", state, last_error)?;
        }
    }
    if assign {
        match refresh_assignment(ctx, shutdown).await {
            Assigned::Done => {}
            Assigned::Revoked(serial) => {
                on_revoked(ctx, &serial, "station-api", state, last_error)?;
            }
            Assigned::Shutdown => return Ok(false),
        }
    }
    Ok(true)
}

/// Sleeps `delay` before the next attempt, ending early for new
/// credentials. `Ok(false)` on shutdown; an error when a revocation stops
/// the daemon.
async fn wait_backoff(
    ctx: &Ctx,
    delay: Duration,
    shutdown: &CancellationToken,
    signals: &mut mpsc::Receiver<Signal>,
    state: &watch::Sender<ConnectionState>,
    last_error: &watch::Sender<Option<String>>,
) -> Result<bool, RunError> {
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(true),
        () = shutdown.cancelled() => Ok(false),
        Some(signal) = signals.recv() => match signal {
            // New credentials: try them now.
            Signal::Renewed { .. } => Ok(true),
            Signal::Revoked { serial } => {
                on_revoked(ctx, &serial, "station-api", state, last_error).map(|()| true)
            }
        },
    }
}
