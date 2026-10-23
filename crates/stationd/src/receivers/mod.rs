// SPDX-License-Identifier: GPL-3.0-only
//! The station's receivers (station-protocol §4.2, §7, §9): the applied
//! set, each receiver's unit and backend, and their states. It lives as long
//! as the daemon: the set outlives every connection (§2.3).
//!
//! - **The set** is `receivers.json` ([`store`]). At start, [`Receivers::start`]
//!   starts every receiver in it, before the tunnel is up.
//!   [`Receivers::configure`] checks a hub's set against the hardware
//!   (`station_proto::validate::check_receiver_set`), writes it, and applies
//!   the difference: a new receiver starts, a changed one restarts, a removed
//!   one stops, an unchanged one is left alone.
//! - **A receiver** is its unit ([`units`]: `coaxhub-radiod@<id>.service`) and
//!   its backend (one [`Backend`] each, from a [`BackendFactory`]). Its state:
//!   - `STARTING` from when it is added, or its unit restarts, until its
//!     backend answers (`Backend::capabilities`);
//!   - `RUNNING` while its unit runs the same invocation;
//!   - `FAILED` when its unit failed or stopped, its backend could not be
//!     created, or its SDR doesn't offer its model and profile, with the
//!     reason;
//!   - `NO_DEVICE` while its SDR is not on the bus (its unit stopped); it
//!     starts by itself when the SDR is plugged in.
//!
//!   A hub may configure a receiver whose SDR is not plugged in (§4.2), so
//!   the model is checked against the SDR when it appears.
//!   When the USB devices change, a receiver that can't run stops, and one
//!   that can starts if it was `NO_DEVICE` or `FAILED`.
//! - **The monitor** watches the units every [`ReceiverOptions::poll`] and
//!   the USB devices every [`ReceiverOptions::hardware_poll`] (a udev-free
//!   poll of sysfs), and sends `Event{HARDWARE_CHANGED}` when an SDR comes
//!   or goes.
//! - **Events** ([`RxEvent`]): `RECEIVER_STATE` on every change of state,
//!   `BACKEND_RESTARTED` when a receiver's backend goes (its channels and
//!   subscriptions are gone), `HARDWARE_CHANGED`, and each backend's own
//!   events with its `receiver_id`. Each says whether it stops a running
//!   measurement plan (§4.1). A connection forwards them while it lasts;
//!   with no connection they go nowhere, and the hub reads the state again
//!   when it connects.

pub mod store;
pub mod units;

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use backend_ka9q::backend::{Ka9qBackend, Ka9qConfig};
use futures_util::StreamExt as _;
use station_backend::{Backend, BackendError, FakeBackend, FakeConfig};
use station_proto::validate::{check_receiver_set, predicted_tunable_range, ranges_overlap};
use station_proto::{limits, v1};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;
use tracing::{debug, error, info, warn};

use crate::dispatch::truncate_utf8;
use crate::hardware::{self, Hardware, UsbDevice};
use crate::measure::StopReason;
use crate::profile::ProfileId;

pub use self::store::{Model, StoredReceiver, StoredSet};
pub use self::units::{ExternalUnits, FakeUnits, Systemd, UnitState, Units};

use v1::ReceiverState as State;

// ------------------------------------------------------------------ backends

/// Creates each receiver's backend.
pub trait BackendFactory: Send + Sync + fmt::Debug {
    /// `Hello.backend`: the kind and version of every backend it creates.
    fn info(&self) -> v1::BackendInfo;
    /// The backend of receiver `rx`.
    fn backend(&self, rx: &StoredReceiver) -> Result<Arc<dyn Backend>, BackendError>;
    /// Whether its backends measure (§4.1).
    fn can_measure(&self) -> bool;
}

/// radiod, one per receiver: on the receiver's slot groups, or every
/// receiver on the same `groups` (a development radiod of one's own).
#[derive(Debug, Clone, Default)]
pub struct Ka9qBackends {
    /// Fixed status and data groups instead of the slot's.
    pub groups: Option<(SocketAddr, Option<SocketAddr>)>,
}

impl BackendFactory for Ka9qBackends {
    fn info(&self) -> v1::BackendInfo {
        backend_ka9q::backend::backend_info()
    }

    fn backend(&self, rx: &StoredReceiver) -> Result<Arc<dyn Backend>, BackendError> {
        let (status, data) = self.groups.unwrap_or_else(|| {
            let (status, data) = rx.groups();
            (status, Some(data))
        });
        Ok(Arc::new(Ka9qBackend::new(Ka9qConfig::new(status, data))?))
    }

    fn can_measure(&self) -> bool {
        true
    }
}

/// A [`FakeBackend`] per receiver, shaped by its profile: its centre,
/// sample rate, range and `sdr_model`, and a retune as its only live
/// control. Clones share the backends made.
#[derive(Clone, Default)]
pub struct FakeBackends {
    made: Arc<Mutex<HashMap<u32, FakeBackend>>>,
    wall_clock: Option<station_backend::fake::WallClock>,
}

impl fmt::Debug for FakeBackends {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeBackends")
            .field("made", &self.made)
            .finish_non_exhaustive()
    }
}

impl FakeBackends {
    /// Backends whose noise floor follows `clock` (Unix ms), not the
    /// system's (tests on a paused clock).
    #[must_use]
    pub fn with_wall_clock(mut self, clock: station_backend::fake::WallClock) -> Self {
        self.wall_clock = Some(clock);
        self
    }

    /// The backend made last for receiver `id`.
    pub fn get(&self, id: u32) -> Option<FakeBackend> {
        self.made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// The fake receiver of `rx`.
    pub fn config(rx: &StoredReceiver) -> FakeConfig {
        let p = rx.profile.profile();
        let hardware = p.hardware_range();
        let center = if p.center.is_some() {
            rx.center_frequency_hz
        } else {
            hardware.start_hz / 2 + hardware.end_hz / 2
        };
        let mut config = FakeConfig::receiver(center, p.sample_rate_hz, hardware, p.sdr_model);
        config.capabilities.receiver.frontend = Some(p.live_controls());
        let gain = rx.gain_db.or(p.gain.map(|g| g.default_db)).unwrap_or(0.0);
        config.frontend.gain_db = gain;
        if let Some(f) = config.health.frontend.as_mut() {
            f.gain_db = gain;
        }
        config
    }
}

impl BackendFactory for FakeBackends {
    fn info(&self) -> v1::BackendInfo {
        FakeConfig::default().info
    }

    fn backend(&self, rx: &StoredReceiver) -> Result<Arc<dyn Backend>, BackendError> {
        let mut backend = FakeBackend::new(Self::config(rx));
        if let Some(clock) = &self.wall_clock {
            backend = backend.with_wall_clock(Arc::clone(clock));
        }
        self.made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(rx.receiver_id, backend.clone());
        Ok(Arc::new(backend))
    }

    fn can_measure(&self) -> bool {
        true
    }
}

/// One backend for every receiver (tests of a backend against a station of
/// one receiver).
#[derive(Clone)]
pub struct SharedBackend(pub Arc<dyn Backend>);

impl fmt::Debug for SharedBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SharedBackend")
            .field(&self.0.info())
            .finish()
    }
}

impl BackendFactory for SharedBackend {
    fn info(&self) -> v1::BackendInfo {
        self.0.info()
    }

    fn backend(&self, _rx: &StoredReceiver) -> Result<Arc<dyn Backend>, BackendError> {
        Ok(Arc::clone(&self.0))
    }

    fn can_measure(&self) -> bool {
        self.0.can_measure()
    }
}

/// How the station runs its receivers: their backends and their units.
#[derive(Debug, Clone)]
pub struct Driver {
    /// Each receiver's backend.
    pub backends: Arc<dyn BackendFactory>,
    /// Each receiver's unit.
    pub units: Arc<dyn Units>,
}

impl Driver {
    /// The bundled radiod under systemd, as a packaged station runs.
    pub fn bundled() -> Self {
        Self {
            backends: Arc::new(Ka9qBackends::default()),
            units: Arc::new(Systemd::default()),
        }
    }

    /// Fake backends with fake units (development and tests).
    pub fn fake() -> Self {
        Self {
            backends: Arc::new(FakeBackends::default()),
            units: Arc::new(FakeUnits::default()),
        }
    }
}

// ------------------------------------------------------------------ receivers

/// How often the monitor looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverOptions {
    /// The units and the backends of starting receivers.
    pub poll: Duration,
    /// The USB devices (§9: every 5 s).
    pub hardware_poll: Duration,
}

impl Default for ReceiverOptions {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(1),
            hardware_poll: Duration::from_secs(5),
        }
    }
}

/// Where the receivers live on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverPaths {
    /// `receivers.json`.
    pub store: PathBuf,
    /// The USB devices (`/sys/bus/usb/devices`).
    pub usb_devices: PathBuf,
    /// Whether this is a packaged build (no development profiles).
    pub packaged: bool,
}

/// An event about the receivers, for the connection to forward.
#[derive(Debug, Clone, PartialEq)]
pub struct RxEvent {
    /// The event.
    pub event: v1::Event,
    /// Whether it stops a running measurement plan, and why (§4.1).
    pub stop: Option<StopReason>,
}

/// One receiver as `stationd status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReceiverSummary {
    /// The hub's ID.
    pub receiver_id: u32,
    /// `starting`, `running`, `failed` or `no_device`.
    pub state: String,
    /// Why, for `failed` and `no_device`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The profile (`rtlsdr-v4`).
    pub profile: String,
    /// The SDR's serial (empty: the only RTL-SDR).
    pub sdr_serial: String,
    /// The centre, Hz.
    pub center_frequency_hz: u64,
    /// The local slot.
    pub slot: u8,
}

/// `starting`, `running`, `failed`, `no_device`.
pub fn state_name(state: State) -> &'static str {
    match state {
        State::Starting => "starting",
        State::Running => "running",
        State::Failed => "failed",
        State::NoDevice => "no_device",
        State::Unspecified => "unspecified",
    }
}

const EVENT_BUFFER: usize = 256;

struct Rx {
    config: StoredReceiver,
    state: State,
    detail: String,
    backend: Option<Arc<dyn Backend>>,
    /// The unit's run the backend belongs to (`RUNNING`), or the run that
    /// failed (`FAILED`).
    invocation: Option<String>,
    forward: Option<AbortHandle>,
    cpu: Option<(u64, std::time::Instant)>,
}

impl Rx {
    /// A receiver just added: `STARTING`, never `UNSPECIFIED`, which a
    /// status must not carry (§5).
    fn new(config: StoredReceiver) -> Self {
        Self {
            config,
            state: State::Starting,
            detail: String::new(),
            backend: None,
            invocation: None,
            forward: None,
            cpu: None,
        }
    }

    /// Drops the backend; whether there was one.
    fn retire(&mut self) -> bool {
        if let Some(f) = self.forward.take() {
            f.abort();
        }
        self.invocation = None;
        self.backend.take().is_some()
    }
}

impl Drop for Rx {
    fn drop(&mut self) {
        self.retire();
    }
}

#[derive(Default)]
struct Shared {
    set: Vec<Rx>,
    devices: Vec<UsbDevice>,
}

impl Shared {
    fn get(&mut self, id: u32) -> Option<&mut Rx> {
        self.set.iter_mut().find(|r| r.config.receiver_id == id)
    }
}

struct Inner {
    paths: ReceiverPaths,
    driver: Driver,
    options: ReceiverOptions,
    /// Serialises applying a set and the monitor's work.
    op: tokio::sync::Mutex<()>,
    shared: Mutex<Shared>,
    events: broadcast::Sender<RxEvent>,
    monitor: Mutex<Option<AbortHandle>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(m) = self
            .monitor
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            m.abort();
        }
    }
}

/// The station's receivers; see the [module docs](self). Clones share them.
#[derive(Clone)]
pub struct Receivers {
    inner: Arc<Inner>,
}

impl fmt::Debug for Receivers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receivers")
            .field("paths", &self.inner.paths)
            .field("driver", &self.inner.driver)
            .finish_non_exhaustive()
    }
}

/// Whether a receiver can start on the SDRs plugged in now (§4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    /// It can: its SDR is plugged in and offers its model and profile (or
    /// its development profile needs none).
    Ready,
    /// Its SDR is not plugged in: `NO_DEVICE`, with why.
    Absent(String),
    /// Its SDR is plugged in but can't serve it: `FAILED`, with why.
    Refused(String),
}

/// Where `config`'s SDR is among `devices`, with `set` the whole applied
/// set (§4.2). A hub may name an SDR that is not plugged in, so its model
/// is checked against the SDR here, when it appears: the table must offer
/// the model for that SDR, and the profile for that model. An empty serial
/// means the only RTL-SDR; while another receiver names that SDR's serial,
/// the SDR is that receiver's.
fn placement(config: &StoredReceiver, set: &[StoredReceiver], devices: &[UsbDevice]) -> Placement {
    let detail = |s: String| truncate_utf8(&s, limits::MAX_DETAIL_BYTES);
    if config.profile.profile().dev_only {
        return Placement::Ready;
    }
    let Some(dev) = hardware::device_for(devices, &config.sdr_serial) else {
        let rtlsdrs = devices.iter().filter(|d| d.is_rtlsdr()).count();
        return Placement::Absent(if !config.sdr_serial.is_empty() {
            detail(format!(
                "no SDR with serial {} is plugged in",
                config.sdr_serial
            ))
        } else if rtlsdrs > 1 {
            format!("{rtlsdrs} RTL-SDRs are plugged in; an empty serial needs exactly one")
        } else {
            "the station's only RTL-SDR is not plugged in".to_owned()
        });
    };
    if config.sdr_serial.is_empty()
        && let Some(other) = set.iter().find(|r| {
            r.receiver_id != config.receiver_id
                && !r.sdr_serial.is_empty()
                && r.sdr_serial == dev.serial
        })
    {
        return Placement::Refused(format!(
            "the station's only RTL-SDR is receiver {}'s",
            other.receiver_id
        ));
    }
    let models = hardware::models_for(dev);
    let Some(option) = models
        .iter()
        .find(|m| m.model == config.model.proto() as i32)
    else {
        return Placement::Refused(detail(format!(
            "model {} not offered for its SDR ({}, {:?})",
            config.model.name(),
            dev.usb_id(),
            dev.product
        )));
    };
    let offered = config
        .profile
        .profile()
        .proto
        .is_some_and(|p| option.profiles.contains(&(p as i32)));
    if !offered {
        return Placement::Refused(format!(
            "profile {} not offered for model {}",
            config.profile,
            config.model.name()
        ));
    }
    Placement::Ready
}

impl Receivers {
    /// No receiver yet; [`Receivers::start`] loads the applied set.
    pub fn new(paths: ReceiverPaths, driver: Driver, options: ReceiverOptions) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            inner: Arc::new(Inner {
                paths,
                driver,
                options,
                op: tokio::sync::Mutex::new(()),
                shared: Mutex::new(Shared::default()),
                events,
                monitor: Mutex::new(None),
            }),
        }
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.inner
            .shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// `Hello.backend`.
    pub fn backend_info(&self) -> v1::BackendInfo {
        self.inner.driver.backends.info()
    }

    /// Whether the station measures (`Capabilities.measurements`).
    pub fn can_measure(&self) -> bool {
        self.inner.driver.backends.can_measure()
    }

    /// The events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<RxEvent> {
        self.inner.events.subscribe()
    }

    fn emit(&self, event: v1::Event, stop: Option<StopReason>) {
        // No connection is fine.
        let _ = self.inner.events.send(RxEvent { event, stop });
    }

    /// Starts the receivers of `receivers.json` and the monitor (§9: at
    /// boot, before the station connects).
    pub async fn start(&self) {
        let _op = self.inner.op.lock().await;
        if let Some(dir) = self.inner.paths.store.parent() {
            // The package creates it; a development station's state
            // directory may not have it yet.
            if let Err(e) = std::fs::create_dir_all(dir) {
                warn!(dir = %dir.display(), error = %e, "cannot create the receivers directory");
            }
        }
        let stored = match StoredSet::load(&self.inner.paths.store) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "cannot use the applied receiver set; no receiver runs until the hub sends one");
                StoredSet::default()
            }
        };
        let devices = hardware::scan(&self.inner.paths.usb_devices);
        let packaged = self.inner.paths.packaged;
        {
            let mut s = self.shared();
            s.devices.clone_from(&devices);
            for config in stored.receivers {
                if config.profile.in_build(packaged) {
                    s.set.push(Rx::new(config));
                } else {
                    warn!(receiver_id = config.receiver_id, profile = %config.profile, "this build has no such profile; the receiver is left out");
                }
            }
        }
        let ids: Vec<u32> = self
            .shared()
            .set
            .iter()
            .map(|r| r.config.receiver_id)
            .collect();
        if !ids.is_empty() {
            info!(receivers = ?ids, "starting the applied receivers");
        }
        for id in ids {
            self.bring_up(id, false, &devices).await;
        }
        let me = Arc::downgrade(&self.inner);
        let task = tokio::spawn(monitor(me, self.inner.options));
        *self
            .inner
            .monitor
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(task.abort_handle());
    }

    /// Records `id`'s new state, with a `RECEIVER_STATE` event when it
    /// changed. Entering or leaving `RUNNING` stops a plan (§4.1).
    fn set_state(&self, id: u32, state: State, detail: &str) {
        let old = {
            let mut s = self.shared();
            let Some(rx) = s.get(id) else { return };
            let old = rx.state;
            rx.state = state;
            rx.detail = truncate_utf8(detail, limits::MAX_DETAIL_BYTES);
            old
        };
        if old == state {
            return;
        }
        match state {
            State::Failed | State::NoDevice => {
                warn!(
                    receiver_id = id,
                    state = state_name(state),
                    detail,
                    "receiver"
                );
            }
            _ => info!(receiver_id = id, state = state_name(state), "receiver"),
        }
        let stop = (old == State::Running || state == State::Running)
            .then_some(StopReason::FrontendChanged);
        let detail = if matches!(state, State::Failed | State::NoDevice) {
            truncate_utf8(detail, limits::MAX_DETAIL_BYTES)
        } else {
            String::new()
        };
        self.emit(
            v1::Event {
                kind: v1::event::Kind::ReceiverState as i32,
                receiver_id: Some(id),
                receiver_state: Some(state as i32),
                detail,
                ..Default::default()
            },
            stop,
        );
    }

    /// Drops `id`'s backend; when it had one, `BACKEND_RESTARTED`: its
    /// channels and subscriptions are gone (§4.2).
    fn take_down(&self, id: u32) {
        let had = self.shared().get(id).is_some_and(Rx::retire);
        if had {
            self.emit(
                v1::Event {
                    kind: v1::event::Kind::BackendRestarted as i32,
                    receiver_id: Some(id),
                    ..Default::default()
                },
                Some(StopReason::BackendRestarted),
            );
        }
    }

    /// A new backend for `id`, its events forwarded with its ID; then
    /// `STARTING`. `FAILED` when the factory refuses.
    async fn attach(&self, id: u32) {
        let Some(config) = self.shared().get(id).map(|r| r.config.clone()) else {
            return;
        };
        match self.inner.driver.backends.backend(&config) {
            Ok(backend) => {
                let forward = tokio::spawn(forward_events(
                    backend.events(),
                    id,
                    self.inner.events.clone(),
                ));
                if let Some(rx) = self.shared().get(id) {
                    rx.retire();
                    rx.backend = Some(backend);
                    rx.forward = Some(forward.abort_handle());
                    rx.cpu = None;
                }
                self.set_state(id, State::Starting, "");
            }
            Err(e) => {
                let invocation = match self.inner.driver.units.state(id).await {
                    UnitState::Active { invocation } => Some(invocation),
                    _ => None,
                };
                self.set_state(
                    id,
                    State::Failed,
                    &format!("its backend could not start: {}", e.message),
                );
                if let Some(rx) = self.shared().get(id) {
                    rx.invocation = invocation;
                }
            }
        }
    }

    /// The placement of receiver `id` among `devices`, against the set as
    /// applied now.
    fn placement_of(&self, id: u32, devices: &[UsbDevice]) -> Option<Placement> {
        let s = self.shared();
        let configs: Vec<StoredReceiver> = s.set.iter().map(|r| r.config.clone()).collect();
        let config = configs.iter().find(|c| c.receiver_id == id)?;
        Some(placement(config, &configs, devices))
    }

    /// Starts (or restarts) `id`'s unit and backend when its SDR is
    /// plugged in and can serve it; `NO_DEVICE` or `FAILED` otherwise, with
    /// its unit stopped.
    async fn bring_up(&self, id: u32, restart: bool, devices: &[UsbDevice]) {
        let Some(place) = self.placement_of(id, devices) else {
            return;
        };
        let units = &self.inner.driver.units;
        let (state, detail) = match place {
            Placement::Ready => (None, String::new()),
            Placement::Absent(detail) => (Some(State::NoDevice), detail),
            Placement::Refused(detail) => (Some(State::Failed), detail),
        };
        if let Some(state) = state {
            if let Err(e) = units.stop(id).await {
                debug!(receiver_id = id, error = %e, "stopping the unit of a receiver its SDR can't serve");
            }
            if let Some(rx) = self.shared().get(id) {
                rx.invocation = None;
            }
            self.set_state(id, state, &detail);
            return;
        }
        let asked = if restart {
            units.restart(id).await
        } else {
            units.start(id).await
        };
        match asked {
            Ok(()) => self.attach(id).await,
            Err(detail) => self.set_state(id, State::Failed, &detail),
        }
    }

    /// The applied set, as stored.
    pub fn stored(&self) -> StoredSet {
        StoredSet {
            receivers: self.shared().set.iter().map(|r| r.config.clone()).collect(),
        }
    }

    /// `GetHardware` (§4.2): a fresh scan, each SDR with the receiver it
    /// serves, and every profile of the package.
    pub fn hardware(&self) -> v1::Hardware {
        let hw = Hardware::scan(&self.inner.paths.usb_devices);
        let configs: Vec<StoredReceiver> =
            self.shared().set.iter().map(|r| r.config.clone()).collect();
        let serving: Vec<(u32, String)> = configs
            .iter()
            .filter(|c| c.profile.profile().proto.is_some())
            .filter(|c| placement(c, &configs, &hw.devices) == Placement::Ready)
            .map(|c| (c.receiver_id, c.sdr_serial.clone()))
            .collect();
        hw.report(&serving)
    }

    /// `ConfigureReceivers` (§4.2): checks the whole set, writes it, applies
    /// the difference and answers each receiver's state; or the refusal,
    /// with nothing changed.
    pub async fn configure(
        &self,
        set: &v1::ConfigureReceivers,
    ) -> Result<v1::ReceiversConfigured, v1::Result> {
        let _op = self.inner.op.lock().await;
        let devices = hardware::scan(&self.inner.paths.usb_devices);
        let report = Hardware {
            devices: devices.clone(),
        }
        .report(&[]);
        check_receiver_set(set, &report).map_err(|e| {
            info!(rule = e.code(), "receiver set refused: {e}");
            e.to_result()
        })?;
        let previous = self.stored();
        let next = store::assign(&previous, set);
        if next.receivers.len() != set.receivers.len() {
            return Err(v1::Result {
                code: v1::ResultCode::Unsupported as i32,
                message: "a model or profile this station doesn't have".to_owned(),
            });
        }
        if let Err(e) = next.save(&self.inner.paths.store) {
            error!(error = %e, "cannot write the receiver set");
            return Err(v1::Result {
                code: v1::ResultCode::BackendError as i32,
                message: truncate_utf8(
                    &format!("cannot store the receiver set: {e}"),
                    limits::MAX_DETAIL_BYTES,
                ),
            });
        }
        let units = &self.inner.driver.units;
        for old in &previous.receivers {
            let id = old.receiver_id;
            if next.get(id).is_none() {
                info!(receiver_id = id, "receiver removed");
                self.take_down(id);
                if let Err(e) = units.stop(id).await {
                    warn!(receiver_id = id, error = %e, "cannot stop a removed receiver's unit");
                }
                let mut s = self.shared();
                s.set.retain(|r| r.config.receiver_id != id);
            }
        }
        // The whole set first, then the starts: a receiver's SDR depends on
        // the others' serials (`placement`).
        let mut starts = Vec::new();
        for config in &next.receivers {
            let id = config.receiver_id;
            match previous.get(id) {
                Some(old) if old.same_config(config) => {}
                Some(_) => {
                    info!(receiver_id = id, "receiver changed: restarting it");
                    self.take_down(id);
                    if let Some(rx) = self.shared().get(id) {
                        rx.config = config.clone();
                    }
                    starts.push((id, true));
                }
                None => {
                    info!(receiver_id = id, profile = %config.profile, slot = config.slot, "receiver added");
                    self.shared().set.push(Rx::new(config.clone()));
                    starts.push((id, false));
                }
            }
        }
        for (id, restart) in starts {
            self.bring_up(id, restart, &devices).await;
        }
        let mut s = self.shared();
        let order: Vec<u32> = next.receivers.iter().map(|r| r.receiver_id).collect();
        s.set.sort_by_key(|r| {
            order
                .iter()
                .position(|id| *id == r.config.receiver_id)
                .unwrap_or(usize::MAX)
        });
        Ok(v1::ReceiversConfigured {
            receivers: s
                .set
                .iter()
                .map(|r| v1::ReceiverStatus {
                    receiver_id: r.config.receiver_id,
                    state: r.state as i32,
                    detail: if matches!(r.state, State::Failed | State::NoDevice) {
                        r.detail.clone()
                    } else {
                        String::new()
                    },
                })
                .collect(),
        })
    }

    /// The backend of `id` while it is `RUNNING`.
    pub fn running_backend(&self, id: u32) -> Option<Arc<dyn Backend>> {
        let mut s = self.shared();
        let rx = s.get(id)?;
        if rx.state == State::Running {
            rx.backend.clone()
        } else {
            None
        }
    }

    /// The running receivers and their backends, in set order.
    pub fn running(&self) -> Vec<(u32, Arc<dyn Backend>)> {
        self.shared()
            .set
            .iter()
            .filter(|r| r.state == State::Running)
            .filter_map(|r| Some((r.config.receiver_id, Arc::clone(r.backend.as_ref()?))))
            .collect()
    }

    /// Receiver `id` as applied.
    pub fn config(&self, id: u32) -> Option<StoredReceiver> {
        self.shared().get(id).map(|r| r.config.clone())
    }

    /// `GetCapabilities` (§4.2): the running receivers in frequency order,
    /// each within the package's claims, and the station-wide limits.
    pub async fn capabilities(&self) -> v1::Capabilities {
        let running: Vec<(u32, ProfileId, Arc<dyn Backend>)> = {
            let s = self.shared();
            s.set
                .iter()
                .filter(|r| r.state == State::Running)
                .filter_map(|r| {
                    Some((
                        r.config.receiver_id,
                        r.config.profile,
                        Arc::clone(r.backend.as_ref()?),
                    ))
                })
                .collect()
        };
        let mut receivers = Vec::with_capacity(running.len());
        let mut modes: Option<Vec<i32>> = None;
        let mut codecs: Option<Vec<i32>> = None;
        let mut waterfall: Option<v1::WaterfallLimits> = None;
        // One preset file serves every radiod, so the receivers agree; if
        // they don't (or one doesn't know), nothing is claimed.
        let mut cw_pitch: Option<u32> = None;
        for (id, profile, backend) in running {
            let caps = match backend.capabilities().await {
                Ok(c) => c,
                Err(e) => {
                    debug!(receiver_id = id, error = %e, "a running receiver did not answer for its capabilities");
                    continue;
                }
            };
            receivers.push(receiver_capabilities(id, profile, caps.receiver));
            let keep = |acc: &mut Option<Vec<i32>>, these: &[i32]| match acc {
                Some(a) => a.retain(|v| these.contains(v)),
                None => *acc = Some(these.to_vec()),
            };
            keep(&mut modes, &caps.modes);
            keep(&mut codecs, &caps.codecs);
            cw_pitch = Some(match cw_pitch {
                Some(acc) if acc != caps.cw_pitch_hz => 0,
                _ => caps.cw_pitch_hz,
            });
            if let Some(w) = caps.waterfall {
                waterfall = Some(match waterfall {
                    Some(acc) => v1::WaterfallLimits {
                        max_bins: acc.max_bins.min(w.max_bins),
                        min_row_interval_ms: acc.min_row_interval_ms.max(w.min_row_interval_ms),
                        max_subscriptions: 0,
                    },
                    None => w,
                });
            }
        }
        receivers.sort_by_key(|r| r.tunable_range.map_or(0, |t| t.start_hz));
        let claim = limits::waterfall_subscriptions_claim(receivers.len());
        v1::Capabilities {
            receivers,
            modes: modes.unwrap_or_default(),
            codecs: codecs.unwrap_or_default(),
            waterfall: waterfall.map(|w| v1::WaterfallLimits {
                max_bins: w.max_bins.min(limits::MAX_BINS),
                min_row_interval_ms: w.min_row_interval_ms,
                max_subscriptions: claim,
            }),
            measurements: self.can_measure().then_some(v1::MeasurementLimits {
                max_noise_bands: limits::MAX_NOISE_BANDS,
                method_version: limits::MEASURE_METHOD_VERSION,
            }),
            cw_pitch_hz: cw_pitch.unwrap_or(0),
        }
    }

    /// Whether receiver `id` may be retuned to `center_hz` (§4.2): inside
    /// its profile's centre range, its tunable range clear of the other
    /// receivers'.
    pub fn check_retune(&self, id: u32, center_hz: u64) -> Result<(), BackendError> {
        let s = self.shared();
        let rx = s
            .set
            .iter()
            .find(|r| r.config.receiver_id == id)
            .ok_or_else(|| BackendError::not_found(format!("receiver {id} is not running")))?;
        let p = rx.config.profile.profile();
        let Some(range) = p.center else {
            return Err(BackendError::unsupported(
                "this receiver's front end cannot retune",
            ));
        };
        if !range.contains(center_hz) {
            return Err(BackendError::out_of_range(format!(
                "receiver {id}: centre {center_hz} Hz outside its profile's centre range"
            )));
        }
        let next = predicted_tunable_range(center_hz, p.tunable_span_hz);
        for other in s.set.iter().filter(|r| r.config.receiver_id != id) {
            let op = other.config.profile.profile();
            let theirs =
                predicted_tunable_range(other.config.center_frequency_hz, op.tunable_span_hz);
            if ranges_overlap(&next, &theirs) {
                return Err(BackendError::invalid_argument(format!(
                    "receiver {id}: tunable range would overlap receiver {}'s",
                    other.config.receiver_id
                )));
            }
        }
        Ok(())
    }

    /// Records receiver `id`'s new centre after a retune, in `receivers.json`
    /// too, so a restart keeps it (§9 `SetFrontend`).
    pub async fn retuned(&self, id: u32, center_hz: u64) {
        let _op = self.inner.op.lock().await;
        let set = {
            let mut s = self.shared();
            let Some(rx) = s.get(id) else { return };
            rx.config.center_frequency_hz = center_hz;
            StoredSet {
                receivers: s.set.iter().map(|r| r.config.clone()).collect(),
            }
        };
        if let Err(e) = set.save(&self.inner.paths.store) {
            warn!(receiver_id = id, error = %e, "cannot store the receiver's new centre");
        }
    }

    /// `StationStatus.receivers` (§4.2): every configured receiver, with
    /// `channels(id)` open on it and its backend's CPU since the last call.
    pub fn health(&self, channels: impl Fn(u32) -> u32) -> Vec<v1::ReceiverHealth> {
        let units = &self.inner.driver.units;
        let mut s = self.shared();
        s.set
            .iter_mut()
            .take(limits::MAX_RECEIVERS)
            .map(|rx| {
                let id = rx.config.receiver_id;
                let health = rx.backend.as_ref().map(|b| b.health());
                let backend = health.as_ref().map_or_else(
                    || v1::BackendHealth {
                        state: v1::backend_health::State::Down as i32,
                        version: String::new(),
                    },
                    |h| v1::BackendHealth {
                        state: match h.backend.state {
                            0 => v1::backend_health::State::Down as i32,
                            s => s,
                        },
                        version: truncate_utf8(&h.backend.version, limits::MAX_VERSION_BYTES),
                    },
                );
                let frontend = (rx.state == State::Running)
                    .then(|| health.and_then(|h| h.frontend))
                    .flatten()
                    .map(crate::status::sanitize_frontend_health);
                let now = std::time::Instant::now();
                let usage = units.cpu_usage_usec(id);
                let backend_cpu = match (usage, rx.cpu) {
                    (Some(u), Some((before, at))) if u >= before => {
                        let wall = now.duration_since(at).as_secs_f64();
                        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                        let share = ((u - before) as f64 / 1e6 / wall) as f32;
                        (wall > 0.0 && share.is_finite()).then_some(share)
                    }
                    _ => None,
                };
                rx.cpu = usage.map(|u| (u, now));
                v1::ReceiverHealth {
                    receiver_id: id,
                    state: rx.state as i32,
                    detail: if matches!(rx.state, State::Failed | State::NoDevice) {
                        rx.detail.clone()
                    } else {
                        String::new()
                    },
                    backend: Some(backend),
                    frontend,
                    active_channels: channels(id),
                    backend_cpu,
                }
            })
            .collect()
    }

    /// Every configured receiver, for `status.json`.
    pub fn summaries(&self) -> Vec<ReceiverSummary> {
        self.shared()
            .set
            .iter()
            .map(|r| ReceiverSummary {
                receiver_id: r.config.receiver_id,
                state: state_name(r.state).to_owned(),
                detail: r.detail.clone(),
                profile: r.config.profile.to_string(),
                sdr_serial: r.config.sdr_serial.clone(),
                center_frequency_hz: r.config.center_frequency_hz,
                slot: r.config.slot,
            })
            .collect()
    }

    /// One look by the monitor: the USB devices when `scan`, then each
    /// receiver's unit and backend.
    async fn tick(&self, scan: bool) {
        let _op = self.inner.op.lock().await;
        if scan {
            self.rescan().await;
        }
        let ids: Vec<(u32, State)> = self
            .shared()
            .set
            .iter()
            .map(|r| (r.config.receiver_id, r.state))
            .collect();
        for (id, state) in ids {
            match state {
                State::Starting => self.watch_starting(id).await,
                State::Running => self.watch_running(id).await,
                State::Failed => self.watch_failed(id).await,
                State::NoDevice | State::Unspecified => {}
            }
        }
    }

    async fn rescan(&self) {
        let devices = hardware::scan(&self.inner.paths.usb_devices);
        let changed = {
            let mut s = self.shared();
            let changed = s.devices != devices;
            s.devices.clone_from(&devices);
            changed
        };
        if !changed {
            return;
        }
        info!(
            sdrs = hardware::sdrs(&devices).len(),
            "the USB devices changed"
        );
        self.emit(
            v1::Event {
                kind: v1::event::Kind::HardwareChanged as i32,
                ..Default::default()
            },
            None,
        );
        // Receivers that can't run any more stop first, so an SDR one of
        // them held is free for the receivers that start.
        let (configs, states): (Vec<StoredReceiver>, Vec<State>) = self
            .shared()
            .set
            .iter()
            .map(|r| (r.config.clone(), r.state))
            .unzip();
        let mut starts = Vec::new();
        for (config, state) in configs.iter().zip(states) {
            let id = config.receiver_id;
            if placement(config, &configs, &devices) == Placement::Ready {
                // Back, or tried again: a receiver that failed may have
                // failed for want of its SDR.
                if matches!(state, State::NoDevice | State::Failed) {
                    starts.push(id);
                }
            } else {
                self.take_down(id);
                self.bring_up(id, false, &devices).await;
            }
        }
        for id in starts {
            info!(receiver_id = id, "the receiver's SDR is plugged in");
            self.bring_up(id, false, &devices).await;
        }
    }

    async fn watch_starting(&self, id: u32) {
        match self.inner.driver.units.state(id).await {
            UnitState::Active { invocation } => {
                let Some(backend) = self.shared().get(id).and_then(|r| r.backend.clone()) else {
                    return;
                };
                if backend.capabilities().await.is_ok() {
                    if let Some(rx) = self.shared().get(id) {
                        rx.invocation = Some(invocation);
                    }
                    self.set_state(id, State::Running, "");
                }
            }
            UnitState::Activating => {}
            UnitState::Failed { detail } => {
                self.take_down(id);
                self.set_state(id, State::Failed, &detail);
            }
            UnitState::Inactive => {
                self.take_down(id);
                self.set_state(id, State::Failed, "its unit stopped");
            }
        }
    }

    async fn watch_running(&self, id: u32) {
        let unit = self.inner.driver.units.state(id).await;
        let same = |inv: &str| {
            self.shared()
                .get(id)
                .is_some_and(|r| r.invocation.as_deref() == Some(inv))
        };
        match unit {
            UnitState::Active { invocation } if same(&invocation) => {}
            UnitState::Active { .. } | UnitState::Activating => {
                info!(receiver_id = id, "the receiver's backend restarted");
                self.take_down(id);
                self.attach(id).await;
            }
            UnitState::Failed { detail } => {
                self.take_down(id);
                self.set_state(id, State::Failed, &detail);
            }
            UnitState::Inactive => {
                self.take_down(id);
                self.set_state(id, State::Failed, "its unit stopped");
            }
        }
    }

    async fn watch_failed(&self, id: u32) {
        let retry = match self.inner.driver.units.state(id).await {
            UnitState::Active { invocation } => self
                .shared()
                .get(id)
                .is_some_and(|r| r.invocation.as_deref() != Some(invocation.as_str())),
            UnitState::Activating => true,
            UnitState::Failed { .. } | UnitState::Inactive => false,
        };
        if retry {
            self.attach(id).await;
        }
    }
}

/// `ReceiverCapabilities` of receiver `id` from its backend's: its ID, at
/// most the package's channel claim, its profile's full range as the
/// hardware range, strings within the caps.
fn receiver_capabilities(
    id: u32,
    profile: ProfileId,
    mut r: v1::ReceiverCapabilities,
) -> v1::ReceiverCapabilities {
    r.receiver_id = id;
    r.max_channels = r.max_channels.min(limits::CHANNELS_PER_RECEIVER);
    r.sdr_model = truncate_utf8(&r.sdr_model, limits::MAX_SDR_MODEL_BYTES);
    let clip = |v: u64| v.min(limits::MAX_FREQUENCY_HZ);
    if let Some(t) = r.tunable_range.as_mut() {
        t.end_hz = clip(t.end_hz);
        t.start_hz = t.start_hz.min(t.end_hz);
    }
    let profile_range = profile.profile().hardware_range();
    let tunable = r.tunable_range.unwrap_or(profile_range);
    let backend_range = r.hardware_range.unwrap_or(tunable);
    r.hardware_range = Some(v1::FrequencyRange {
        start_hz: profile_range
            .start_hz
            .min(backend_range.start_hz)
            .min(tunable.start_hz),
        end_hz: clip(
            profile_range
                .end_hz
                .max(backend_range.end_hz)
                .max(tunable.end_hz),
        ),
    });
    r.tunable_range = Some(tunable);
    if let Some(f) = r.frontend.as_mut() {
        f.antenna_ports.truncate(limits::MAX_ANTENNA_PORTS);
        for port in &mut f.antenna_ports {
            *port = truncate_utf8(port, limits::MAX_ANTENNA_PORT_BYTES);
        }
        for v in [
            &mut f.min_gain_db,
            &mut f.max_gain_db,
            &mut f.max_attenuation_db,
        ] {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
    }
    r
}

/// Forwards a backend's events, with its receiver's ID where the event
/// names a receiver.
async fn forward_events(
    mut events: station_backend::EventStream,
    receiver_id: u32,
    out: broadcast::Sender<RxEvent>,
) {
    use v1::event::Kind;
    while let Some(mut event) = events.next().await {
        let kind = Kind::try_from(event.kind).ok();
        if matches!(
            kind,
            Some(
                Kind::FrontendOverload
                    | Kind::DeviceLost
                    | Kind::DeviceRestored
                    | Kind::BackendRestarted
                    | Kind::FrontendChanged
            )
        ) {
            event.receiver_id = Some(receiver_id);
        }
        let stop = match kind {
            Some(Kind::FrontendChanged) => Some(StopReason::FrontendChanged),
            Some(Kind::BackendRestarted) => Some(StopReason::BackendRestarted),
            _ => None,
        };
        let _ = out.send(RxEvent { event, stop });
    }
}

/// The monitor task: [`Receivers::tick`] every `poll`, scanning the USB
/// devices every `hardware_poll`, until the receivers are dropped.
async fn monitor(inner: Weak<Inner>, options: ReceiverOptions) {
    let mut ticker = tokio::time::interval(options.poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_scan = tokio::time::Instant::now();
    loop {
        ticker.tick().await;
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let scan = last_scan.elapsed() >= options.hardware_poll;
        if scan {
            last_scan = tokio::time::Instant::now();
        }
        Receivers { inner }.tick(scan).await;
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
