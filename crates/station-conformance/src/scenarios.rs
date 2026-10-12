// SPDX-License-Identifier: MIT
//! Named conformance scenarios. Each starts a fresh [`FakeHub`] and test
//! PKI, starts the station under test against it, scripts the hub side,
//! and finally stops the station, which must exit cleanly (unless the
//! scenario says otherwise).
//!
//! Every station message in every scenario also passes the hub-side §5
//! validator, so e.g. an unsolicited or duplicate response, audio for a
//! channel the hub never opened, a receiver the hub never configured or a
//! malformed `Capabilities` fails any scenario.
//!
//! The station runs on simulated SDRs ([`SimulatedSdr`]) that the scenarios
//! plug into [`Target::usb_devices`]: by default [`standard_sdrs`], a
//! generic RTL-SDR and an RTL-SDR Blog V4, which [`standard_set`] makes
//! receivers 1 (40 m) and 2 (20 m).
//!
//! Timing expectations come from [`ScenarioConfig`]: the defaults fit the
//! production timings of station-protocol §2.1/§2.2 (slow: backoff up to
//! 60 s); [`ScenarioConfig::fast`] fits a station configured with short
//! test intervals.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use station_proto::validate::DropReason;
use station_proto::{limits, v1};
use tokio::time::Instant;

use crate::error::{ConformanceError, Result, ensure};
use crate::hub::{FakeHub, HubTls, StationConn, TlsVersions};
use crate::pki::TestPki;
use crate::sut::{SimulatedSdr, StationUnderTest, Target};

use v1::request::Command;
use v1::response::Payload;
use v1::station_message::Body;

/// Timing expectations for the station under test.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    /// First connection after start.
    pub connect_within: Duration,
    /// Reconnection after a disconnect (the station's largest backoff plus
    /// connect time).
    pub reconnect_within: Duration,
    /// Any response (§2.1: 5 s).
    pub response_within: Duration,
    /// How long "nothing happens" is observed.
    pub quiet_period: Duration,
    /// `Welcome.status_interval_ms` sent by the hub.
    pub status_interval_ms: u32,
    /// `Goodbye.retry_after_ms` in `honours_goodbye_retry_after`.
    pub retry_after_ms: u32,
    /// How long a revoked station is watched for reconnection attempts.
    pub revoked_quiet: Duration,
    /// Exit after a stop request or a revocation.
    pub exit_within: Duration,
    /// Detection of a silent (frozen) hub; `None` skips
    /// `detects_dead_connection`. Production: HTTP/2 keepalive 20 s + 10 s.
    pub dead_peer_within: Option<Duration>,
    /// A receiver's backend starting, until it is `RUNNING` (§4.2).
    pub receiver_start_within: Duration,
    /// The station noticing an SDR plugged in or removed
    /// (`Event{HARDWARE_CHANGED}`, §4.2): a poll every 5 s in production.
    pub hardware_within: Duration,
    /// How long one minute of the station's measurement schedule takes
    /// (§4.1): 60 s on the system clock, less for a station under test on
    /// an accelerated clock. `None` skips the measurement scenarios that
    /// wait for reports.
    pub measure_minute: Option<Duration>,
}

impl Default for ScenarioConfig {
    /// Production timings (§2.1, §2.2).
    fn default() -> Self {
        Self {
            connect_within: Duration::from_secs(15),
            reconnect_within: limits::BACKOFF_MAX + Duration::from_secs(10),
            response_within: limits::RESPONSE_TIMEOUT,
            quiet_period: Duration::from_millis(500),
            status_interval_ms: 1000,
            retry_after_ms: 1500,
            revoked_quiet: Duration::from_secs(5),
            exit_within: Duration::from_secs(10),
            dead_peer_within: Some(
                limits::HTTP2_KEEPALIVE_INTERVAL
                    + limits::HTTP2_KEEPALIVE_TIMEOUT
                    + Duration::from_secs(5),
            ),
            receiver_start_within: Duration::from_secs(30),
            hardware_within: Duration::from_secs(12),
            measure_minute: Some(Duration::from_secs(60)),
        }
    }
}

impl ScenarioConfig {
    /// For a station configured with test timings: backoff at most ~300 ms,
    /// HTTP/2 keepalive ~300 ms, receivers and hardware watched every
    /// ~100 ms.
    pub fn fast() -> Self {
        Self {
            connect_within: Duration::from_secs(5),
            reconnect_within: Duration::from_secs(3),
            response_within: Duration::from_secs(3),
            quiet_period: Duration::from_millis(400),
            status_interval_ms: 200,
            retry_after_ms: 1000,
            revoked_quiet: Duration::from_millis(1500),
            exit_within: Duration::from_secs(5),
            dead_peer_within: Some(Duration::from_secs(3)),
            receiver_start_within: Duration::from_secs(3),
            hardware_within: Duration::from_secs(3),
            // A station on a clock 60 times as fast.
            measure_minute: Some(Duration::from_secs(1)),
        }
    }
}

/// A scenario's future.
pub type ScenarioFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

/// A named scenario.
#[derive(Clone, Copy)]
pub struct Scenario {
    /// Stable name (also the function name in this module).
    pub name: &'static str,
    /// What it checks.
    pub summary: &'static str,
    /// Runs it.
    pub run: for<'a> fn(&'a mut dyn StationUnderTest, &'a ScenarioConfig) -> ScenarioFuture<'a>,
}

impl fmt::Debug for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scenario")
            .field("name", &self.name)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

macro_rules! scenario {
    ($name:ident, $summary:expr) => {
        Scenario {
            name: stringify!($name),
            summary: $summary,
            run: |sut, cfg| Box::pin($name(sut, cfg)),
        }
    };
}

/// Every scenario, in a sensible order.
pub const ALL: &[Scenario] = &[
    scenario!(
        hello_then_welcome,
        "Hello first, nothing else before Welcome (§2)"
    ),
    scenario!(
        answers_get_capabilities,
        "Capabilities valid: the running receivers in frequency order, the package's claims (§4, §4.2, §7)"
    ),
    scenario!(answers_ping, "Pong echoes the nonce (§4)"),
    scenario!(
        answers_every_request_once,
        "a burst of mixed requests gets exactly one response each (§4)"
    ),
    scenario!(
        rejects_unknown_command_as_unsupported,
        "unknown commands and enum values get UNSUPPORTED, no disconnect (§3)"
    ),
    scenario!(
        admin_op_ok_or_unsupported,
        "AdminOp RESTART_BACKEND is OK or UNSUPPORTED, never refused otherwise (§4)"
    ),
    scenario!(
        set_frontend_on_a_receiver,
        "SetFrontend names a running receiver: NOT_FOUND otherwise, UNSUPPORTED for a control it lacks, a retune answered for it (§4, §4.2)"
    ),
    scenario!(
        status_at_interval,
        "StationStatus every Welcome.status_interval_ms, every configured receiver once (§2, §5)"
    ),
    scenario!(
        reconnects_after_hub_restart,
        "reconnects with the same boot_id after the hub restarts (§2.2, NFR-5)"
    ),
    scenario!(
        reconnects_after_connection_drop,
        "reconnects after the TCP connection is cut (§2.2)"
    ),
    scenario!(
        honours_goodbye_retry_after,
        "waits at least Goodbye.retry_after_ms before reconnecting (§2.2)"
    ),
    scenario!(
        stops_on_revoked,
        "Goodbye{REVOKED}: never reconnects and exits unsuccessfully (§2.2)"
    ),
    scenario!(
        sends_shutting_down_on_stop,
        "graceful stop sends Event{SHUTTING_DOWN} and closes the stream"
    ),
    scenario!(
        rejects_untrusted_gateway,
        "refuses a gateway certificate from another CA (§1)"
    ),
    scenario!(
        rejects_tls12_gateway,
        "refuses a gateway that only speaks TLS 1.2"
    ),
    scenario!(
        detects_dead_connection,
        "detects a silent hub (HTTP/2 keepalive) and reconnects (§2.1)"
    ),
    scenario!(
        opens_channel_and_streams_audio,
        "OpenChannel on a receiver → audio for that channel; CloseChannel stops it; an unknown receiver is NOT_FOUND (§4, §6.1)"
    ),
    scenario!(
        channel_limit_per_receiver,
        "a receiver's channels stop at its max_channels with RESOURCE_EXHAUSTED; another receiver still opens (§4, §7)"
    ),
    scenario!(
        subscribes_waterfall_and_streams_rows,
        "SubscribeWaterfall on a receiver → valid rows; UnsubscribeWaterfall stops them (§4, §6.2)"
    ),
    scenario!(
        drops_channels_on_disconnect,
        "channels do not survive a reconnect (§2.3)"
    ),
    scenario!(
        reports_hardware,
        "GetHardware: the SDRs in port order with the table's models and profiles, a V4 offered only RTLSDR_V4, no non-SDR device, every profile of the package listed (§4.2)"
    ),
    scenario!(
        applies_receiver_set,
        "ConfigureReceivers → ReceiversConfigured, RECEIVER_STATE to RUNNING, the receivers in Capabilities and Hardware (§4.2)"
    ),
    scenario!(
        refuses_invalid_receiver_sets,
        "a shared serial twice, an empty serial among several RTL-SDRs, a profile not offered, overlapping ranges, a centre out of range: refused, naming the receiver, nothing changed (§4.2)"
    ),
    scenario!(
        applies_set_naming_an_absent_sdr,
        "a set naming an SDR that is not plugged in: accepted, NO_DEVICE; plugged in: HARDWARE_CHANGED and it runs (§4.2)"
    ),
    scenario!(
        unchanged_set_keeps_channels,
        "the same set again restarts nothing: channels stay open, audio continues (§4.2)"
    ),
    scenario!(
        changed_and_removed_receivers,
        "a changed receiver restarts and a removed one stops, closing their channels with BACKEND_RESTARTED (§4.2)"
    ),
    scenario!(
        sdr_unplugged_and_back,
        "an SDR removed: HARDWARE_CHANGED and NO_DEVICE; plugged back: it runs again (§4.2)"
    ),
    scenario!(
        receivers_survive_reconnect_and_restart,
        "the applied set runs across a reconnect and a station restart, before the hub sends it again (§2.3, §7)"
    ),
    scenario!(
        measures_when_asked,
        "a plan over two receivers: MeasurementsStarted for both, then idle-duty reports per receiver for measuring minutes only (§4.1)"
    ),
    scenario!(
        duty_follows_listeners,
        "a listener channel makes its receiver's duty continuous at a minute boundary, the other receiver's stays idle (§4.1)"
    ),
    scenario!(
        stops_measuring_on_frontend_change,
        "SetFrontend stops the plan with Event{MEASUREMENT_STOPPED} (§4.1)"
    ),
    scenario!(
        one_plan_per_connection,
        "a second plan is ALREADY_EXISTS, StopMeasurements stops reports, a plan does not survive a reconnect (§4.1, §2.3)"
    ),
];

/// The scenario called `name`.
pub fn by_name(name: &str) -> Option<&'static Scenario> {
    ALL.iter().find(|s| s.name == name)
}

/// The SDRs plugged in for most scenarios: a generic RTL-SDR (serial
/// `CH000001`) at `1-1.2` and an RTL-SDR Blog V4 (`CH000002`) at `1-1.3`.
pub fn standard_sdrs() -> Vec<SimulatedSdr> {
    vec![
        SimulatedSdr::rtlsdr("1-1.2", "CH000001"),
        SimulatedSdr::blog_v4("1-1.3", "CH000002"),
    ]
}

/// A receiver of a set.
pub fn receiver(
    receiver_id: u32,
    sdr_serial: &str,
    model: v1::SdrModel,
    profile: v1::SdrProfile,
    center_frequency_hz: u64,
) -> v1::ReceiverConfig {
    v1::ReceiverConfig {
        receiver_id,
        sdr_serial: sdr_serial.to_owned(),
        model: model as i32,
        profile: profile as i32,
        center_frequency_hz,
        gain_db: None,
        hardware_agc: None,
        attenuation_db: None,
    }
}

/// The receiver set for [`standard_sdrs`]: receiver 1 on the generic
/// RTL-SDR by direct sampling at 7.1 MHz (40 m), receiver 2 on the V4 at
/// 14.2 MHz (20 m).
pub fn standard_set() -> v1::ConfigureReceivers {
    v1::ConfigureReceivers {
        receivers: vec![
            receiver(
                1,
                "CH000001",
                v1::SdrModel::RtlsdrDirectSampling,
                v1::SdrProfile::RtlsdrHf,
                7_100_000,
            ),
            receiver(
                2,
                "CH000002",
                v1::SdrModel::RtlsdrBlogV4,
                v1::SdrProfile::RtlsdrV4,
                14_200_000,
            ),
        ],
    }
}

/// A fake hub, a test PKI and the files the station needs.
#[derive(Debug)]
pub struct Harness {
    /// The fake hub.
    pub hub: FakeHub,
    /// The PKI the station certificate comes from.
    pub pki: TestPki,
    /// What the station is started with.
    pub target: Target,
    _dir: tempfile::TempDir,
}

#[allow(clippy::needless_pass_by_value)] // used as map_err(io_err)
fn io_err(e: std::io::Error) -> ConformanceError {
    ConformanceError::Hub(format!("writing test files: {e}"))
}

impl Harness {
    /// A harness with [`standard_sdrs`] plugged in, whose hub uses the
    /// PKI's own certificate (TLS 1.3).
    pub async fn new() -> Result<Self> {
        Self::with_sdrs(&standard_sdrs()).await
    }

    /// A harness with `sdrs` plugged in.
    pub async fn with_sdrs(sdrs: &[SimulatedSdr]) -> Result<Self> {
        let h = Self::with_hub_tls(TestPki::hub_tls).await?;
        for sdr in sdrs {
            h.plug(sdr)?;
        }
        Ok(h)
    }

    /// A harness without SDRs whose hub TLS comes from `hub_tls` (the
    /// station always trusts only `pki.ca_pem`).
    pub async fn with_hub_tls(hub_tls: impl FnOnce(&TestPki) -> HubTls) -> Result<Self> {
        let pki = TestPki::generate()?;
        let hub = FakeHub::start(&hub_tls(&pki)).await?;
        let dir = tempfile::tempdir().map_err(io_err)?;
        let path = |name: &str| dir.path().join(name);
        std::fs::write(path("gateway-ca.pem"), &pki.ca_pem).map_err(io_err)?;
        std::fs::write(path("station.pem"), &pki.station_cert_pem).map_err(io_err)?;
        write_private(&path("station.key"), &pki.station_key_pem)?;
        let work_dir = path("work");
        std::fs::create_dir(&work_dir).map_err(io_err)?;
        let usb_devices = path("usb-devices");
        std::fs::create_dir(&usb_devices).map_err(io_err)?;
        let target = Target {
            gateway_url: hub.gateway_url(),
            gateway_ca: path("gateway-ca.pem"),
            cert: path("station.pem"),
            key: path("station.key"),
            station_id: pki.station_id,
            usb_devices,
            work_dir,
        };
        Ok(Self {
            hub,
            pki,
            target,
            _dir: dir,
        })
    }

    /// Plugs `sdr` into the station.
    pub fn plug(&self, sdr: &SimulatedSdr) -> Result<()> {
        sdr.plug(&self.target.usb_devices).map_err(io_err)
    }

    /// Removes the device at `port` from the station.
    pub fn unplug(&self, port: &str) -> Result<()> {
        SimulatedSdr::unplug(&self.target.usb_devices, port).map_err(io_err)
    }

    /// Accepts the station's next stream, expects `Hello`, sends `Welcome`.
    pub async fn connect(&mut self, within: Duration, cfg: &ScenarioConfig) -> Result<StationConn> {
        let mut conn = self.hub.accept(within).await?;
        ensure(conn.station_id() == Some(self.target.station_id), || {
            format!(
                "the station presented certificate for {:?}, expected {}",
                conn.station_id(),
                self.target.station_id
            )
        })?;
        conn.expect_hello(cfg.response_within).await?;
        conn.welcome(cfg.status_interval_ms).await?;
        Ok(conn)
    }

    /// Connects as a hub does (§2): `GetHardware`, `ConfigureReceivers`
    /// with `set`, the receivers `RUNNING`, then `GetCapabilities`.
    pub async fn connect_with(
        &mut self,
        within: Duration,
        set: &v1::ConfigureReceivers,
        cfg: &ScenarioConfig,
    ) -> Result<(StationConn, v1::Capabilities)> {
        let mut conn = self.connect(within, cfg).await?;
        get_hardware(&mut conn, cfg).await?;
        let configured = configured(&configure(&mut conn, set.clone(), cfg).await?)?;
        let ids: Vec<u32> = set.receivers.iter().map(|r| r.receiver_id).collect();
        wait_running(&mut conn, &configured, &ids, cfg.receiver_start_within).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        Ok((conn, caps))
    }

    /// [`Harness::connect_with`] the [`standard_set`]; the capabilities
    /// list both receivers.
    pub async fn ready(&mut self, cfg: &ScenarioConfig) -> Result<(StationConn, v1::Capabilities)> {
        let (conn, caps) = self
            .connect_with(cfg.connect_within, &standard_set(), cfg)
            .await?;
        let ids: Vec<u32> = caps.receivers.iter().map(|r| r.receiver_id).collect();
        ensure(ids == [1, 2], || {
            format!("Capabilities.receivers {ids:?}, expected [1, 2] (in frequency order)")
        })?;
        Ok((conn, caps))
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_err)?;
    f.write_all(contents.as_bytes()).map_err(io_err)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(io_err)
}

/// How a scenario script hands over to the harness.
#[derive(Debug)]
pub enum After {
    /// Stop the station (it must exit successfully); drain this stream
    /// meanwhile, if any.
    Stop(Option<Box<StationConn>>),
    /// The station already stopped (or its exit was checked by the script).
    Exited,
}

fn station_err(e: impl fmt::Display) -> ConformanceError {
    ConformanceError::Station(e.to_string())
}

/// Runs `script` against a started station, then stops the station.
///
/// This is the building block of every scenario in [`ALL`]; custom
/// scenarios can use it too. The script gets the station under test for
/// scenarios that stop it or wait for its exit themselves.
pub async fn run_scenario(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
    harness: Result<Harness>,
    script: impl AsyncFnOnce(&mut Harness, &mut dyn StationUnderTest) -> Result<After>,
) -> Result<()> {
    let mut h = harness?;
    sut.start(&h.target).await.map_err(station_err)?;
    match script(&mut h, &mut *sut).await {
        Ok(After::Stop(conn)) => {
            let exit = stop_draining(sut, conn, cfg).await?;
            ensure(exit.success, || {
                format!(
                    "the station did not exit cleanly after a stop request: {}",
                    exit.detail
                )
            })
        }
        Ok(After::Exited) => sut.stop().await.map(drop).map_err(station_err),
        Err(e) => {
            let _ = sut.stop().await;
            Err(e)
        }
    }
}

/// Stops the station while draining `conn`.
async fn stop_draining(
    sut: &mut dyn StationUnderTest,
    conn: Option<Box<StationConn>>,
    cfg: &ScenarioConfig,
) -> Result<crate::sut::Exit> {
    let drain = async {
        if let Some(mut c) = conn {
            let _ = c.expect_closed(cfg.exit_within).await;
        }
    };
    let (exit, ()) = tokio::join!(sut.stop(), drain);
    exit.map_err(station_err)
}

fn code(r: &v1::Response) -> Option<v1::ResultCode> {
    r.result
        .as_ref()
        .and_then(|res| v1::ResultCode::try_from(res.code).ok())
}

fn expect_code(r: &v1::Response, expected: v1::ResultCode, what: &str) -> Result<()> {
    ensure(code(r) == Some(expected), || {
        format!(
            "{what}: expected {}, got {:?}",
            expected.as_str_name(),
            r.result
        )
    })
}

fn capabilities(r: &v1::Response) -> Result<v1::Capabilities> {
    expect_code(r, v1::ResultCode::Ok, "GetCapabilities")?;
    match &r.payload {
        Some(Payload::Capabilities(c)) => Ok(c.clone()),
        other => Err(ConformanceError::Unexpected {
            expected: "Capabilities payload".into(),
            got: format!("{other:?}"),
        }),
    }
}

fn get_capabilities() -> Command {
    Command::GetCapabilities(v1::GetCapabilities {})
}

fn ping(nonce: u64) -> Command {
    Command::Ping(v1::Ping {
        nonce,
        hub_time_unix_ms: 0,
    })
}

fn admin_op(kind: v1::AdminOpKind, receiver_id: Option<u32>) -> Command {
    Command::AdminOp(v1::AdminOp {
        kind: kind as i32,
        receiver_id,
    })
}

async fn expect_pong(conn: &mut StationConn, nonce: u64, cfg: &ScenarioConfig) -> Result<()> {
    let r = conn.call(ping(nonce), cfg.response_within).await?;
    expect_code(&r, v1::ResultCode::Ok, "Ping")?;
    match r.payload {
        Some(Payload::Pong(p)) if p.nonce == nonce => Ok(()),
        other => Err(ConformanceError::Unexpected {
            expected: format!("Pong{{nonce: {nonce}}}"),
            got: format!("{other:?}"),
        }),
    }
}

/// `GetHardware` → the station's `Hardware`.
pub async fn get_hardware(conn: &mut StationConn, cfg: &ScenarioConfig) -> Result<v1::Hardware> {
    let r = conn
        .call(
            Command::GetHardware(v1::GetHardware {}),
            cfg.response_within,
        )
        .await?;
    expect_code(&r, v1::ResultCode::Ok, "GetHardware")?;
    match r.payload {
        Some(Payload::Hardware(h)) => Ok(h),
        other => Err(ConformanceError::Unexpected {
            expected: "Hardware payload".into(),
            got: format!("{other:?}"),
        }),
    }
}

/// Sends `ConfigureReceivers` with `set`; returns the response.
pub async fn configure(
    conn: &mut StationConn,
    set: v1::ConfigureReceivers,
    cfg: &ScenarioConfig,
) -> Result<v1::Response> {
    conn.call(Command::ConfigureReceivers(set), cfg.response_within)
        .await
}

/// The `ReceiversConfigured` of an `OK` answer.
pub fn configured(r: &v1::Response) -> Result<v1::ReceiversConfigured> {
    expect_code(r, v1::ResultCode::Ok, "ConfigureReceivers")?;
    match &r.payload {
        Some(Payload::ReceiversConfigured(c)) => Ok(c.clone()),
        other => Err(ConformanceError::Unexpected {
            expected: "ReceiversConfigured payload".into(),
            got: format!("{other:?}"),
        }),
    }
}

fn events(msgs: &[(Instant, v1::StationMessage)], kind: v1::event::Kind) -> Vec<v1::Event> {
    msgs.iter()
        .filter_map(|(_, m)| match &m.body {
            Some(Body::Event(e)) if e.kind == kind as i32 => Some(e.clone()),
            _ => None,
        })
        .collect()
}

/// The `RECEIVER_STATE` events in `msgs`, as (receiver, state).
fn receiver_states(msgs: &[(Instant, v1::StationMessage)]) -> Vec<(u32, i32)> {
    events(msgs, v1::event::Kind::ReceiverState)
        .iter()
        .filter_map(|e| Some((e.receiver_id?, e.receiver_state?)))
        .collect()
}

/// Whether `msgs` hold a `RECEIVER_STATE` event of `receiver_id` in
/// `state`.
fn has_state(
    msgs: &[(Instant, v1::StationMessage)],
    receiver_id: u32,
    state: v1::ReceiverState,
) -> bool {
    receiver_states(msgs).contains(&(receiver_id, state as i32))
}

/// Waits until every receiver of `ids` is `RUNNING`: in `configured`
/// already, or by a `RECEIVER_STATE` event. Returns what arrived meanwhile.
pub async fn wait_running(
    conn: &mut StationConn,
    configured: &v1::ReceiversConfigured,
    ids: &[u32],
    within: Duration,
) -> Result<Vec<(Instant, v1::StationMessage)>> {
    let running = v1::ReceiverState::Running as i32;
    let pending: Vec<u32> = ids
        .iter()
        .copied()
        .filter(|id| {
            !configured
                .receivers
                .iter()
                .any(|r| r.receiver_id == *id && r.state == running)
        })
        .collect();
    let got = collect_until(conn, within, within / 20, |m| {
        pending
            .iter()
            .all(|&id| has_state(m, id, v1::ReceiverState::Running))
    })
    .await?;
    let missing: Vec<u32> = pending
        .iter()
        .copied()
        .filter(|&id| !has_state(&got, id, v1::ReceiverState::Running))
        .collect();
    ensure(missing.is_empty(), || {
        format!(
            "receivers {missing:?} not RUNNING within {within:?} (states seen: {:?})",
            receiver_states(&got)
        )
    })?;
    Ok(got)
}

/// Polls `GetCapabilities` until `pred` holds or `within` passes.
async fn poll_capabilities(
    conn: &mut StationConn,
    cfg: &ScenarioConfig,
    within: Duration,
    pred: impl Fn(&v1::Capabilities) -> bool,
) -> Result<v1::Capabilities> {
    let deadline = Instant::now() + within;
    loop {
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        if pred(&caps) || Instant::now() >= deadline {
            return Ok(caps);
        }
        tokio::time::sleep(within / 20).await;
    }
}

fn receiver_ids(caps: &v1::Capabilities) -> Vec<u32> {
    caps.receivers.iter().map(|r| r.receiver_id).collect()
}

fn receiver_caps(caps: &v1::Capabilities, receiver_id: u32) -> Option<&v1::ReceiverCapabilities> {
    caps.receivers.iter().find(|r| r.receiver_id == receiver_id)
}

/// Hello comes first, with sane fields; the station sends nothing else
/// before `Welcome`; the session then works.
pub async fn hello_then_welcome(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.hub.accept(cfg.connect_within).await?;
        ensure(conn.station_id() == Some(h.target.station_id), || {
            "the client certificate does not carry the station's ID".into()
        })?;
        let hello = conn.expect_hello(cfg.response_within).await?;
        let version = hello.protocol_version.unwrap_or_default();
        ensure(
            version.major == station_proto::PROTOCOL_VERSION.major,
            || {
                format!(
                    "Hello.protocol_version.major is {}, expected {}",
                    version.major,
                    station_proto::PROTOCOL_VERSION.major
                )
            },
        )?;
        ensure(!hello.daemon_version.is_empty(), || {
            "Hello.daemon_version is empty".into()
        })?;
        ensure(!hello.platform.is_empty(), || {
            "Hello.platform is empty".into()
        })?;
        let early = conn.collect(cfg.quiet_period).await?;
        ensure(early.is_empty(), || {
            format!("the station sent {} message(s) before Welcome", early.len())
        })?;
        conn.welcome(cfg.status_interval_ms).await?;
        expect_pong(&mut conn, 1, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `GetCapabilities` answers valid capabilities: no receiver before a set,
/// then the running receivers in frequency order, each with a channel limit
/// within the package's claim and its tunable range around its centre, and
/// waterfall subscriptions for the running receivers (§4.2, §7).
pub async fn answers_get_capabilities(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(caps.receivers.is_empty(), || {
            format!(
                "a station without receivers lists {:?}",
                receiver_ids(&caps)
            )
        })?;
        drop(conn);
        let (conn, caps) = h.ready(cfg).await?;
        for (rx, set) in caps.receivers.iter().zip(&standard_set().receivers) {
            ensure(
                (1..=limits::CHANNELS_PER_RECEIVER).contains(&rx.max_channels),
                || {
                    format!(
                        "receiver {}: max_channels {}, the claim is 1..={}",
                        rx.receiver_id,
                        rx.max_channels,
                        limits::CHANNELS_PER_RECEIVER
                    )
                },
            )?;
            let range = rx.tunable_range.unwrap_or_default();
            ensure(
                range.start_hz <= set.center_frequency_hz
                    && set.center_frequency_hz <= range.end_hz,
                || {
                    format!(
                        "receiver {}: tunable range {range:?} does not hold its centre {}",
                        rx.receiver_id, set.center_frequency_hz
                    )
                },
            )?;
        }
        let ranges: Vec<u64> = caps
            .receivers
            .iter()
            .map(|r| r.tunable_range.unwrap_or_default().start_hz)
            .collect();
        ensure(ranges.windows(2).all(|w| w[0] <= w[1]), || {
            format!("receivers not in frequency order: {ranges:?}")
        })?;
        if let Some(w) = caps.waterfall {
            ensure(
                w.max_subscriptions <= limits::waterfall_subscriptions_claim(caps.receivers.len()),
                || {
                    format!(
                        "waterfall.max_subscriptions {} above the claim for {} receivers",
                        w.max_subscriptions,
                        caps.receivers.len()
                    )
                },
            )?;
        }
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `Ping` gets `Pong` with the same nonce.
pub async fn answers_ping(sut: &mut dyn StationUnderTest, cfg: &ScenarioConfig) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        for nonce in [0, 1, 0xDEAD_BEEF, u64::MAX] {
            expect_pong(&mut conn, nonce, cfg).await?;
        }
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// A burst of mixed requests, all outstanding at once, gets exactly one
/// response each and nothing more.
pub async fn answers_every_request_once(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let mut ids = Vec::new();
        for i in 0..5u32 {
            ids.push(conn.request(get_capabilities()).await?);
            ids.push(conn.request(ping(u64::from(i))).await?);
            ids.push(
                conn.request(Command::GetHardware(v1::GetHardware {}))
                    .await?,
            );
            ids.push(
                conn.request(Command::CloseChannel(v1::CloseChannel {
                    channel_id: 1000 + i,
                }))
                .await?,
            );
            ids.push(
                conn.request(Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall {
                    subscription_id: 2000 + i,
                }))
                .await?,
            );
        }
        ids.push(conn.request_raw(None).await?);
        ids.push(
            conn.request(Command::SetFrontend(v1::SetFrontend {
                receiver_id: 77,
                ..Default::default()
            }))
            .await?,
        );
        ensure(ids.len() <= limits::MAX_OUTSTANDING_REQUESTS, || {
            "script bug: too many requests".into()
        })?;
        for id in &ids {
            let r = conn.expect_response(*id, cfg.response_within).await?;
            ensure(r.result.is_some(), || {
                format!("response {id} has no result")
            })?;
        }
        // Any further response is unsolicited: the validator fails `collect`.
        let extra = conn.collect(cfg.quiet_period).await?;
        let responses = extra
            .iter()
            .filter(|(_, m)| matches!(m.body, Some(Body::Response(_))))
            .count();
        ensure(responses == 0, || format!("{responses} extra response(s)"))?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// §3: an unknown command or enum value is answered `UNSUPPORTED`; the
/// session continues.
pub async fn rejects_unknown_command_as_unsupported(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let id = conn.request_raw(None).await?;
        let r = conn.expect_response(id, cfg.response_within).await?;
        expect_code(&r, v1::ResultCode::Unsupported, "unknown command")?;
        ensure(r.payload.is_none(), || {
            "an error response carries a payload".into()
        })?;
        let r = conn
            .call(
                Command::AdminOp(v1::AdminOp {
                    kind: 99,
                    receiver_id: None,
                }),
                cfg.response_within,
            )
            .await?;
        expect_code(
            &r,
            v1::ResultCode::Unsupported,
            "AdminOp with an unknown kind",
        )?;
        let open = Command::OpenChannel(v1::OpenChannel {
            channel_id: 7,
            receiver_id: 1,
            frequency_hz: 7_100_000,
            mode: 99,
            low_cut_hz: -3000,
            high_cut_hz: 3000,
            audio: Some(opus_params()),
            squelch_snr_db: None,
        });
        let r = conn.call(open, cfg.response_within).await?;
        expect_code(
            &r,
            v1::ResultCode::Unsupported,
            "OpenChannel with an unknown mode",
        )?;
        let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            subscription_id: 3,
            receiver_id: 1,
            range: Some(v1::FrequencyRange {
                start_hz: 7_000_000,
                end_hz: 7_200_000,
            }),
            bins: 256,
            row_interval_ms: 100,
            quantization: None,
            compression: 99,
        });
        let r = conn.call(sub, cfg.response_within).await?;
        expect_code(
            &r,
            v1::ResultCode::Unsupported,
            "SubscribeWaterfall with unknown compression",
        )?;
        let mut set = standard_set();
        set.receivers[1].profile = 99;
        let r = configure(&mut conn, set, cfg).await?;
        expect_code(
            &r,
            v1::ResultCode::Unsupported,
            "ConfigureReceivers with an unknown profile",
        )?;
        expect_pong(&mut conn, 5, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `AdminOp{RESTART_BACKEND}`, for every receiver and for one, is `OK` (the
/// station performs it) or `UNSUPPORTED` (it doesn't implement it), never
/// refused otherwise: there is no station-side list of allowed operations.
/// The other kinds would end the run (a restart, an update, a reboot) and
/// are not sent.
pub async fn admin_op_ok_or_unsupported(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, _) = h.ready(cfg).await?;
        for receiver_id in [None, Some(1)] {
            let r = conn
                .call(
                    admin_op(v1::AdminOpKind::RestartBackend, receiver_id),
                    cfg.response_within,
                )
                .await?;
            ensure(
                matches!(
                    code(&r),
                    Some(v1::ResultCode::Ok | v1::ResultCode::Unsupported)
                ),
                || format!("AdminOp RESTART_BACKEND({receiver_id:?}): {:?}", r.result),
            )?;
        }
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `SetFrontend` names a running receiver (`NOT_FOUND` for another), is
/// `UNSUPPORTED` for a control the receiver's `FrontendControls` don't
/// offer live, and a retune inside its profile is answered with the
/// receiver's new `FrontendState`.
pub async fn set_frontend_on_a_receiver(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        let set = |f: fn(&mut v1::SetFrontend)| {
            let mut req = v1::SetFrontend::default();
            f(&mut req);
            Command::SetFrontend(req)
        };
        let r = conn
            .call(set(|q| q.receiver_id = 9), cfg.response_within)
            .await?;
        expect_code(&r, v1::ResultCode::NotFound, "SetFrontend on receiver 9")?;
        let Some(rx) = receiver_caps(&caps, 1) else {
            return Err(ConformanceError::Assertion(
                "receiver 1 is not running".into(),
            ));
        };
        let controls = rx.frontend.clone().unwrap_or_default();
        if !controls.has_attenuator {
            let r = conn
                .call(
                    set(|q| {
                        q.receiver_id = 1;
                        q.attenuation_db = Some(10.0);
                    }),
                    cfg.response_within,
                )
                .await?;
            expect_code(
                &r,
                v1::ResultCode::Unsupported,
                "SetFrontend attenuation on a receiver without an attenuator",
            )?;
        }
        if controls.can_retune {
            let center = 7_200_000;
            let r = conn
                .call(
                    Command::SetFrontend(v1::SetFrontend {
                        receiver_id: 1,
                        center_frequency_hz: Some(center),
                        ..Default::default()
                    }),
                    cfg.response_within,
                )
                .await?;
            expect_code(&r, v1::ResultCode::Ok, "SetFrontend retune")?;
            let Some(Payload::FrontendState(state)) = r.payload else {
                return Err(ConformanceError::Unexpected {
                    expected: "FrontendState payload".into(),
                    got: format!("{:?}", r.payload),
                });
            };
            let range = state.tunable_range.unwrap_or_default();
            ensure(range.start_hz <= center && center <= range.end_hz, || {
                format!("retuned to {center}, the tunable range is {range:?}")
            })?;
        }
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `StationStatus` arrives about every `status_interval_ms`, never faster
/// than the §5 rate limit; with receivers, each configured one once (the
/// validator checks it).
pub async fn status_at_interval(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, _) = h.ready(cfg).await?;
        let interval = Duration::from_millis(u64::from(cfg.status_interval_ms));
        let window = interval * 5 + interval / 2;
        let got = conn.collect(window).await?;
        let statuses: Vec<(Instant, &v1::StationStatus)> = got
            .iter()
            .filter_map(|(t, m)| match &m.body {
                Some(Body::Status(s)) => Some((*t, s)),
                _ => None,
            })
            .collect();
        ensure((4..=7).contains(&statuses.len()), || {
            format!(
                "{} StationStatus in {window:?} at a {interval:?} interval",
                statuses.len()
            )
        })?;
        for pair in statuses.windows(2) {
            let gap = pair[1].0.saturating_duration_since(pair[0].0);
            ensure(gap >= interval / 2, || {
                format!("two StationStatus only {gap:?} apart")
            })?;
        }
        if let Some((_, last)) = statuses.last() {
            let mut ids: Vec<u32> = last.receivers.iter().map(|r| r.receiver_id).collect();
            ids.sort_unstable();
            ensure(ids == [1, 2], || {
                format!("StationStatus.receivers {ids:?}, expected both receivers")
            })?;
        }
        let rate_drops = conn
            .drops()
            .iter()
            .filter(|d| matches!(d, DropReason::RateLimited { .. }))
            .count();
        ensure(rate_drops == 0, || {
            format!("{rate_drops} message(s) over the §5 rate limits")
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// The hub stops (connections killed, port closed) and comes back on the
/// same port: the station reconnects on its own, same `boot_id`.
pub async fn reconnects_after_hub_restart(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let conn = h.connect(cfg.connect_within, cfg).await?;
        let boot_id = conn.hello().map(|hello| hello.boot_id);
        h.hub.stop().await;
        drop(conn);
        // Let the station fail a few attempts against the closed port.
        tokio::time::sleep(cfg.quiet_period).await;
        h.hub.restart().await?;
        let mut conn = h.connect(cfg.reconnect_within, cfg).await?;
        ensure(conn.hello().map(|hello| hello.boot_id) == boot_id, || {
            "boot_id changed across a reconnect of the same process".into()
        })?;
        expect_pong(&mut conn, 2, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// The TCP connection is cut: the station reconnects.
pub async fn reconnects_after_connection_drop(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let conn = h.connect(cfg.connect_within, cfg).await?;
        let boot_id = conn.hello().map(|hello| hello.boot_id);
        conn.kill();
        drop(conn);
        let mut conn = h.connect(cfg.reconnect_within, cfg).await?;
        ensure(conn.hello().map(|hello| hello.boot_id) == boot_id, || {
            "boot_id changed".into()
        })?;
        expect_pong(&mut conn, 3, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `Goodbye{SHUTDOWN, retry_after_ms}`: no reconnect before `retry_after_ms`.
pub async fn honours_goodbye_retry_after(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let retry_after = Duration::from_millis(u64::from(cfg.retry_after_ms));
        let sent = Instant::now();
        conn.goodbye(v1::goodbye::Reason::Shutdown, cfg.retry_after_ms)
            .await?;
        conn.expect_closed(cfg.response_within).await?;
        let conn = h.connect(retry_after + cfg.reconnect_within, cfg).await?;
        let waited = sent.elapsed();
        ensure(waited >= retry_after, || {
            format!("reconnected after {waited:?}, before retry_after_ms = {retry_after:?}")
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `Goodbye{REVOKED}`: the station closes the stream, never reconnects and
/// exits unsuccessfully.
pub async fn stops_on_revoked(sut: &mut dyn StationUnderTest, cfg: &ScenarioConfig) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        conn.goodbye(v1::goodbye::Reason::Revoked, 0).await?;
        conn.expect_closed(cfg.response_within).await?;
        let exit = sut.wait_exit(cfg.exit_within).await.map_err(station_err)?;
        let exit = exit.ok_or_else(|| {
            ConformanceError::Assertion(format!(
                "the station is still running {:?} after Goodbye{{REVOKED}}",
                cfg.exit_within
            ))
        })?;
        ensure(!exit.success, || {
            "the station exited successfully after being revoked".into()
        })?;
        h.hub.expect_no_connection(cfg.revoked_quiet).await?;
        Ok(After::Exited)
    })
    .await
}

/// A graceful stop sends `Event{SHUTTING_DOWN}`, then closes the stream,
/// and the station exits successfully.
pub async fn sends_shutting_down_on_stop(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        expect_pong(&mut conn, 9, cfg).await?;
        let (exit, sent) = tokio::join!(sut.stop(), async {
            let sent = conn.expect_closed(cfg.exit_within).await;
            // Close the hub side too, so the station need not wait.
            conn.close();
            sent
        });
        let sent = sent?;
        let exit = exit.map_err(station_err)?;
        let shutting_down = sent.iter().any(|m| {
            matches!(&m.body, Some(Body::Event(e)) if e.kind == v1::event::Kind::ShuttingDown as i32)
        });
        ensure(shutting_down, || "no Event{SHUTTING_DOWN} before the stream closed".into())?;
        ensure(exit.success, || format!("unclean exit after stop: {}", exit.detail))?;
        Ok(After::Exited)
    })
    .await
}

/// The hub's certificate comes from a CA the station does not trust: the
/// TLS handshake fails, no stream is ever opened.
pub async fn rejects_untrusted_gateway(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let other = TestPki::generate()?;
    let harness = Harness::with_hub_tls(|pki| HubTls {
        cert_chain_pem: other.hub_cert_pem.clone(),
        key_pem: other.hub_key_pem.clone(),
        client_ca_pem: pki.ca_pem.clone(),
        versions: TlsVersions::Tls13Only,
    })
    .await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        h.hub.next_tls_failure(cfg.connect_within).await?;
        h.hub.expect_no_connection(cfg.quiet_period).await?;
        Ok(After::Stop(None))
    })
    .await
}

/// A gateway that only speaks TLS 1.2 is refused.
pub async fn rejects_tls12_gateway(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::with_hub_tls(|pki| HubTls {
        versions: TlsVersions::Tls12Only,
        ..pki.hub_tls()
    })
    .await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        h.hub.next_tls_failure(cfg.connect_within).await?;
        h.hub.expect_no_connection(cfg.quiet_period).await?;
        Ok(After::Stop(None))
    })
    .await
}

/// The hub goes silent without closing anything: the station notices
/// (HTTP/2 keepalive, §2.1) and reconnects.
pub async fn detects_dead_connection(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let Some(detect_within) = cfg.dead_peer_within else {
        return Ok(());
    };
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let conn = h.connect(cfg.connect_within, cfg).await?;
        conn.freeze();
        let mut next = h.connect(detect_within + cfg.reconnect_within, cfg).await?;
        conn.kill();
        drop(conn);
        expect_pong(&mut next, 4, cfg).await?;
        Ok(After::Stop(Some(Box::new(next))))
    })
    .await
}

fn opus_params() -> v1::AudioParams {
    v1::AudioParams {
        codec: v1::AudioCodec::Opus as i32,
        bitrate_bps: 16_000,
        sample_rate_hz: 12_000,
        frame_duration_ms: 20,
    }
}

/// An `OpenChannel` in the middle of `receiver_id`'s tunable range, or
/// `None` if it doesn't run or has no audio.
fn open_command(caps: &v1::Capabilities, receiver_id: u32, channel_id: u32) -> Option<Command> {
    let rx = receiver_caps(caps, receiver_id)?;
    let range = rx.tunable_range?;
    let mode = *caps.modes.first()?;
    if rx.max_channels == 0 || !caps.codecs.contains(&(v1::AudioCodec::Opus as i32)) {
        return None;
    }
    Some(Command::OpenChannel(v1::OpenChannel {
        channel_id,
        receiver_id,
        frequency_hz: range.start_hz + (range.end_hz - range.start_hz) / 2,
        mode,
        low_cut_hz: -3000,
        high_cut_hz: 3000,
        audio: Some(opus_params()),
        squelch_snr_db: None,
    }))
}

fn audio_frames(msgs: &[(Instant, v1::StationMessage)], channel_id: u32) -> usize {
    msgs.iter()
        .filter(|(_, m)| matches!(&m.body, Some(Body::Audio(a)) if a.channel_id == channel_id))
        .count()
}

async fn open(conn: &mut StationConn, command: Command, cfg: &ScenarioConfig) -> Result<()> {
    let r = conn.call(command, cfg.response_within).await?;
    expect_code(&r, v1::ResultCode::Ok, "OpenChannel")?;
    ensure(matches!(r.payload, Some(Payload::ChannelOpened(_))), || {
        "no ChannelOpened".into()
    })
}

fn missing_receiver(receiver_id: u32) -> ConformanceError {
    ConformanceError::Assertion(format!("receiver {receiver_id} offers no audio channel"))
}

/// Audio flows for a channel opened on a receiver and stops after
/// `CloseChannel`; a channel on a receiver that doesn't run is `NOT_FOUND`.
pub async fn opens_channel_and_streams_audio(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        let Some(Command::OpenChannel(mut elsewhere)) = open_command(&caps, 2, 5) else {
            return Err(missing_receiver(2));
        };
        elsewhere.receiver_id = 9;
        let r = conn
            .call(Command::OpenChannel(elsewhere), cfg.response_within)
            .await?;
        expect_code(&r, v1::ResultCode::NotFound, "OpenChannel on receiver 9")?;
        let open_1 = open_command(&caps, 1, 1).ok_or_else(|| missing_receiver(1))?;
        open(&mut conn, open_1, cfg).await?;
        // 20 ms frames: at least a few in half a second (plus slack).
        let got = conn.collect(Duration::from_millis(600)).await?;
        let n = audio_frames(&got, 1);
        ensure(n >= 3, || format!("only {n} audio frame(s) in 600 ms"))?;
        let r = conn
            .call(
                Command::CloseChannel(v1::CloseChannel { channel_id: 1 }),
                cfg.response_within,
            )
            .await?;
        expect_code(&r, v1::ResultCode::Ok, "CloseChannel")?;
        // Frames in flight may still arrive briefly (the hub's 5 s grace).
        let _ = conn.collect(Duration::from_millis(200)).await?;
        let late = conn.collect(cfg.quiet_period).await?;
        let n = audio_frames(&late, 1);
        ensure(n == 0, || format!("{n} audio frame(s) after CloseChannel"))?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// A receiver takes channels up to its `max_channels`; one more is
/// `RESOURCE_EXHAUSTED`, and the other receiver still opens a channel.
pub async fn channel_limit_per_receiver(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        let max = receiver_caps(&caps, 1).map_or(0, |r| r.max_channels);
        for channel_id in 1..=max {
            let command = open_command(&caps, 1, channel_id).ok_or_else(|| missing_receiver(1))?;
            open(&mut conn, command, cfg).await?;
        }
        let one_more = open_command(&caps, 1, max + 1).ok_or_else(|| missing_receiver(1))?;
        let r = conn.call(one_more, cfg.response_within).await?;
        expect_code(
            &r,
            v1::ResultCode::ResourceExhausted,
            "OpenChannel beyond the receiver's max_channels",
        )?;
        let other = open_command(&caps, 2, max + 2).ok_or_else(|| missing_receiver(2))?;
        open(&mut conn, other, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// Rows flow for a subscription on a receiver (each decompresses to exactly
/// `bins`, checked by the validator) and stop after `UnsubscribeWaterfall`.
pub async fn subscribes_waterfall_and_streams_rows(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        let range = receiver_caps(&caps, 2).and_then(|r| r.tunable_range);
        let (Some(range), Some(w)) = (range, caps.waterfall) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        if w.max_subscriptions == 0 || w.max_bins < limits::MIN_BINS {
            return Ok(After::Stop(Some(Box::new(conn))));
        }
        let row_interval_ms = w.min_row_interval_ms.max(100);
        let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            subscription_id: 1,
            receiver_id: 2,
            range: Some(range),
            bins: w.max_bins.min(256),
            row_interval_ms,
            quantization: Some(v1::Quantization {
                offset_db: -130.0,
                step_db: 0.5,
            }),
            compression: v1::WaterfallCompression::Zstd as i32,
        });
        let r = conn.call(sub, cfg.response_within).await?;
        expect_code(&r, v1::ResultCode::Ok, "SubscribeWaterfall")?;
        let window = Duration::from_millis(u64::from(row_interval_ms) * 4 + 200);
        let got = conn.collect(window).await?;
        let rows = got
            .iter()
            .filter(|(_, m)| matches!(&m.body, Some(Body::Waterfall(r)) if r.subscription_id == 1))
            .count();
        ensure(rows >= 2, || {
            format!("only {rows} waterfall row(s) in {window:?}")
        })?;
        let un = Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 1 });
        let r = conn.call(un, cfg.response_within).await?;
        expect_code(&r, v1::ResultCode::Ok, "UnsubscribeWaterfall")?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// A channel opened on one connection is gone on the next: no audio for
/// it (the validator would flag it), and its ID can be opened again.
pub async fn drops_channels_on_disconnect(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        let open_1 = open_command(&caps, 1, 1).ok_or_else(|| missing_receiver(1))?;
        open(&mut conn, open_1.clone(), cfg).await?;
        conn.kill();
        drop(conn);
        let (mut conn, _) = h
            .connect_with(cfg.reconnect_within, &standard_set(), cfg)
            .await?;
        // Audio for channel 1 now would be a violation (never opened here).
        let got = conn.collect(cfg.quiet_period).await?;
        ensure(audio_frames(&got, 1) == 0, || {
            "audio for a channel of the previous connection".into()
        })?;
        open(&mut conn, open_1, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

// ------------------------------------------------------------------ receivers (§4.2)

fn models_of(sdr: &v1::Sdr) -> HashMap<i32, HashSet<i32>> {
    sdr.models
        .iter()
        .map(|m| (m.model, m.profiles.iter().copied().collect()))
        .collect()
}

/// `GetHardware`: the SDRs in port order, each RTL-SDR with the models and
/// profiles of the table (a V4 offered only `RTLSDR_V4`, a generic RTL-SDR
/// never), an SDR the station doesn't support reported too, a USB hub not
/// at all; every profile listed once, every profile offered among them,
/// each default centre in its range. A fresh station names no receiver.
pub async fn reports_hardware(sut: &mut dyn StationUnderTest, cfg: &ScenarioConfig) -> Result<()> {
    let sdrs = [
        SimulatedSdr::hub("usb1"),
        SimulatedSdr::rtlsdr("1-1.2", "00000001"),
        SimulatedSdr::blog_v4("1-1.3", "00000002"),
        SimulatedSdr::airspy("1-1.4", "A00000000001"),
    ];
    let harness = Harness::with_sdrs(&sdrs).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let hw = get_hardware(&mut conn, cfg).await?;
        let ports: Vec<&str> = hw.sdrs.iter().map(|s| s.port.as_str()).collect();
        ensure(ports == ["1-1.2", "1-1.3", "1-1.4"], || {
            format!("Hardware.sdrs at {ports:?}, expected the three SDRs in port order (no hub)")
        })?;
        let (rtl, v4, airspy) = (&hw.sdrs[0], &hw.sdrs[1], &hw.sdrs[2]);
        ensure(
            rtl.usb_vendor_id == 0x0bda
                && rtl.usb_product_id == 0x2838
                && rtl.serial == "00000001"
                && rtl.product == "RTL2838UHIDIR",
            || format!("the RTL-SDR is reported as {rtl:?}"),
        )?;
        ensure(airspy.usb_vendor_id == 0x1d50, || {
            format!("the Airspy is reported as {airspy:?}")
        })?;
        let v4_profile = v1::SdrProfile::RtlsdrV4 as i32;
        let v4_models = models_of(v4);
        ensure(
            v4_models
                == HashMap::from([(
                    v1::SdrModel::RtlsdrBlogV4 as i32,
                    HashSet::from([v4_profile]),
                )]),
            || {
                format!(
                    "a Blog V4 must be offered only RTLSDR_V4, got {:?}",
                    v4.models
                )
            },
        )?;
        let rtl_models = models_of(rtl);
        ensure(
            !rtl_models.is_empty()
                && !rtl_models.contains_key(&(v1::SdrModel::RtlsdrBlogV4 as i32))
                && rtl_models.values().all(|p| !p.contains(&v4_profile)),
            || format!("a generic RTL-SDR offered {:?}", rtl.models),
        )?;
        let offered: HashSet<i32> = hw
            .sdrs
            .iter()
            .flat_map(|s| s.models.iter().flat_map(|m| m.profiles.iter().copied()))
            .collect();
        let listed: Vec<i32> = hw.profiles.iter().map(|p| p.profile).collect();
        let unique: HashSet<i32> = listed.iter().copied().collect();
        ensure(
            unique.len() == listed.len() && offered.iter().all(|p| unique.contains(p)),
            || format!("profiles {listed:?}, offered {offered:?}"),
        )?;
        for p in &hw.profiles {
            let range = p.center_range.unwrap_or_default();
            ensure(
                range.start_hz <= p.default_center_hz
                    && p.default_center_hz <= range.end_hz
                    && p.tunable_span_hz > 0,
                || format!("profile {p:?}"),
            )?;
        }
        ensure(hw.sdrs.iter().all(|s| s.receiver_id.is_none()), || {
            "a fresh station names a receiver in its hardware".into()
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `ConfigureReceivers` with the standard set: `ReceiversConfigured` for
/// both (the validator checks each once), `RECEIVER_STATE` to `RUNNING`,
/// both in `Capabilities` and named on their SDRs in `Hardware`.
pub async fn applies_receiver_set(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        get_hardware(&mut conn, cfg).await?;
        let c = configured(&configure(&mut conn, standard_set(), cfg).await?)?;
        let mut ids: Vec<u32> = c.receivers.iter().map(|r| r.receiver_id).collect();
        ids.sort_unstable();
        ensure(ids == [1, 2], || format!("ReceiversConfigured for {ids:?}"))?;
        wait_running(&mut conn, &c, &[1, 2], cfg.receiver_start_within).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(receiver_ids(&caps) == [1, 2], || {
            format!(
                "Capabilities.receivers {:?}, expected [1, 2]",
                receiver_ids(&caps)
            )
        })?;
        let hw = get_hardware(&mut conn, cfg).await?;
        let serving: Vec<(String, Option<u32>)> = hw
            .sdrs
            .iter()
            .map(|s| (s.serial.clone(), s.receiver_id))
            .collect();
        ensure(
            serving
                == [
                    ("CH000001".to_owned(), Some(1)),
                    ("CH000002".to_owned(), Some(2)),
                ],
            || format!("the SDRs serve {serving:?}"),
        )?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// Receiver `id` on the generic RTL-SDR `serial`, by direct sampling.
fn hf_receiver(id: u32, serial: &str, center_hz: u64) -> v1::ReceiverConfig {
    receiver(
        id,
        serial,
        v1::SdrModel::RtlsdrDirectSampling,
        v1::SdrProfile::RtlsdrHf,
        center_hz,
    )
}

/// Receiver 2 on the V4 `CH000002`.
fn v4_receiver(center_hz: u64) -> v1::ReceiverConfig {
    receiver(
        2,
        "CH000002",
        v1::SdrModel::RtlsdrBlogV4,
        v1::SdrProfile::RtlsdrV4,
        center_hz,
    )
}

/// The sets `refuses_invalid_receiver_sets` sends: what is wrong, the set,
/// the receiver its answer must name, and the code. `hf_max` is the top of
/// the `RTLSDR_HF` profile's centre range.
fn invalid_sets(hf_max: u64) -> Vec<(&'static str, v1::ConfigureReceivers, u32, v1::ResultCode)> {
    let set = |receivers| v1::ConfigureReceivers { receivers };
    let mut v4_hf = v4_receiver(14_200_000);
    v4_hf.profile = v1::SdrProfile::RtlsdrHf as i32;
    let first = hf_receiver(1, "00000001", 7_100_000);
    vec![
        (
            "a shared serial used twice",
            set(vec![first.clone(), hf_receiver(3, "00000001", 21_200_000)]),
            3,
            v1::ResultCode::InvalidArgument,
        ),
        (
            "an empty serial among several RTL-SDRs",
            set(vec![first.clone(), hf_receiver(3, "", 21_200_000)]),
            3,
            v1::ResultCode::InvalidArgument,
        ),
        (
            "a profile the table doesn't offer",
            set(vec![first.clone(), v4_hf]),
            2,
            v1::ResultCode::PermissionDenied,
        ),
        (
            "overlapping ranges",
            set(vec![first, v4_receiver(7_600_000)]),
            2,
            v1::ResultCode::InvalidArgument,
        ),
        (
            "a centre out of range",
            set(vec![
                hf_receiver(1, "00000001", hf_max.saturating_add(1)),
                v4_receiver(14_200_000),
            ]),
            1,
            v1::ResultCode::InvalidArgument,
        ),
    ]
}

/// Refused sets: each answered with its code and a message that names the
/// receiver, and nothing changes: no state event, the same capabilities.
pub async fn refuses_invalid_receiver_sets(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let sdrs = [
        SimulatedSdr::rtlsdr("1-1.2", "00000001"),
        SimulatedSdr::blog_v4("1-1.3", "CH000002"),
        // A second dongle with the factory serial of the first.
        SimulatedSdr::rtlsdr("1-1.4", "00000001"),
    ];
    let harness = Harness::with_sdrs(&sdrs).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let good = v1::ConfigureReceivers {
            receivers: vec![
                hf_receiver(1, "00000001", 7_100_000),
                v4_receiver(14_200_000),
            ],
        };
        let (mut conn, before) = h.connect_with(cfg.connect_within, &good, cfg).await?;
        let hw = get_hardware(&mut conn, cfg).await?;
        let hf_max = hw
            .profiles
            .iter()
            .find(|p| p.profile == v1::SdrProfile::RtlsdrHf as i32)
            .and_then(|p| p.center_range)
            .map_or(u64::MAX, |r| r.end_hz);
        for (what, set, named, expected) in invalid_sets(hf_max) {
            let r = configure(&mut conn, set, cfg).await?;
            expect_code(&r, expected, what)?;
            let message = r.result.map(|r| r.message).unwrap_or_default();
            ensure(message.contains(&named.to_string()), || {
                format!("{what}: the message {message:?} does not name receiver {named}")
            })?;
        }
        let got = conn.collect(cfg.quiet_period).await?;
        let changes = events(&got, v1::event::Kind::ReceiverState).len()
            + events(&got, v1::event::Kind::BackendRestarted).len();
        ensure(changes == 0, || {
            format!("{changes} receiver event(s) after refused sets")
        })?;
        let after = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(after.receivers == before.receivers, || {
            format!(
                "the receivers changed after refused sets: {:?} → {:?}",
                before.receivers, after.receivers
            )
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// The standard set on a station with only the generic RTL-SDR: the
/// hardware still lists the V4's profile, the set is accepted with
/// receiver 2 `NO_DEVICE` and receiver 1 `RUNNING`. The V4 plugged in:
/// `HARDWARE_CHANGED`, receiver 2 `RUNNING` by itself, in `Capabilities`
/// and named on its SDR.
pub async fn applies_set_naming_an_absent_sdr(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::with_sdrs(&standard_sdrs()[..1]).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let hw = get_hardware(&mut conn, cfg).await?;
        let v4_profile = v1::SdrProfile::RtlsdrV4 as i32;
        ensure(hw.profiles.iter().any(|p| p.profile == v4_profile), || {
            format!(
                "Hardware.profiles {:?} without RTLSDR_V4: every profile of the package is listed",
                hw.profiles.iter().map(|p| p.profile).collect::<Vec<_>>()
            )
        })?;
        let c = configured(&configure(&mut conn, standard_set(), cfg).await?)?;
        let no_device = c
            .receivers
            .iter()
            .any(|r| r.receiver_id == 2 && r.state == v1::ReceiverState::NoDevice as i32);
        ensure(no_device, || {
            format!(
                "ReceiversConfigured {:?}: receiver 2's SDR is not plugged in",
                c.receivers
            )
        })?;
        wait_running(&mut conn, &c, &[1], cfg.receiver_start_within).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(receiver_ids(&caps) == [1], || {
            format!(
                "Capabilities.receivers {:?} without receiver 2's SDR",
                receiver_ids(&caps)
            )
        })?;
        h.plug(&standard_sdrs()[1])?;
        let within = cfg.hardware_within + cfg.receiver_start_within;
        let running = |m: &[(Instant, v1::StationMessage)]| {
            !events(m, v1::event::Kind::HardwareChanged).is_empty()
                && has_state(m, 2, v1::ReceiverState::Running)
        };
        let got = collect_until(&mut conn, within, within / 20, running).await?;
        ensure(running(&got), || {
            format!(
                "receiver 2 not RUNNING within {within:?} of its SDR plugged in (states: {:?})",
                receiver_states(&got)
            )
        })?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(receiver_ids(&caps) == [1, 2], || {
            format!(
                "Capabilities.receivers {:?}, expected [1, 2]",
                receiver_ids(&caps)
            )
        })?;
        let hw = get_hardware(&mut conn, cfg).await?;
        ensure(
            hw.sdrs
                .iter()
                .any(|s| s.serial == "CH000002" && s.receiver_id == Some(2)),
            || "the V4 plugged in does not name receiver 2".into(),
        )?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// The same set again is accepted and restarts nothing: no
/// `BACKEND_RESTARTED`, no state change, the channel's audio continues.
pub async fn unchanged_set_keeps_channels(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        let open_1 = open_command(&caps, 1, 1).ok_or_else(|| missing_receiver(1))?;
        open(&mut conn, open_1, cfg).await?;
        let c = configured(&configure(&mut conn, standard_set(), cfg).await?)?;
        ensure(
            c.receivers
                .iter()
                .all(|r| r.state == v1::ReceiverState::Running as i32),
            || format!("an unchanged set answered {:?}", c.receivers),
        )?;
        let got = conn.collect(cfg.quiet_period).await?;
        let restarted = events(&got, v1::event::Kind::BackendRestarted).len()
            + events(&got, v1::event::Kind::ReceiverState).len()
            + events(&got, v1::event::Kind::ChannelClosed).len();
        ensure(restarted == 0, || {
            format!("{restarted} restart, state or close event(s) for an unchanged set")
        })?;
        let n = audio_frames(&got, 1);
        ensure(n >= 3, || {
            format!("only {n} audio frame(s) after the same set")
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// A set that changes receiver 1's centre and leaves receiver 2 out:
/// `BACKEND_RESTARTED` for both, receiver 1 back to `RUNNING` on its new
/// passband, receiver 2 gone from the capabilities, and no audio from
/// either receiver's old channel.
pub async fn changed_and_removed_receivers(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = h.ready(cfg).await?;
        open(
            &mut conn,
            open_command(&caps, 1, 1).ok_or_else(|| missing_receiver(1))?,
            cfg,
        )
        .await?;
        open(
            &mut conn,
            open_command(&caps, 2, 2).ok_or_else(|| missing_receiver(2))?,
            cfg,
        )
        .await?;
        let mut first = standard_set().receivers.remove(0);
        first.center_frequency_hz = 7_300_000;
        let c = configured(
            &configure(
                &mut conn,
                v1::ConfigureReceivers {
                    receivers: vec![first],
                },
                cfg,
            )
            .await?,
        )?;
        let mut got = wait_running(&mut conn, &c, &[1], cfg.receiver_start_within).await?;
        got.extend(conn.collect(cfg.quiet_period).await?);
        let restarted: HashSet<u32> = events(&got, v1::event::Kind::BackendRestarted)
            .iter()
            .filter_map(|e| e.receiver_id)
            .collect();
        ensure(restarted == HashSet::from([1, 2]), || {
            format!("BACKEND_RESTARTED for {restarted:?}, expected receivers 1 and 2")
        })?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(receiver_ids(&caps) == [1], || {
            format!(
                "Capabilities.receivers {:?}, expected [1]",
                receiver_ids(&caps)
            )
        })?;
        let range = receiver_caps(&caps, 1)
            .and_then(|r| r.tunable_range)
            .unwrap_or_default();
        ensure(
            range.start_hz <= 7_300_000 && 7_300_000 <= range.end_hz,
            || format!("receiver 1 runs on {range:?}, not around its new centre"),
        )?;
        let late = conn.collect(cfg.quiet_period).await?;
        ensure(audio_frames(&late, 1) + audio_frames(&late, 2) == 0, || {
            "audio from a channel of a restarted or removed receiver".into()
        })?;
        // The changed receiver opens channels again.
        open(
            &mut conn,
            open_command(&caps, 1, 3).ok_or_else(|| missing_receiver(1))?,
            cfg,
        )
        .await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// Receiver 2's SDR is unplugged: `HARDWARE_CHANGED`, `RECEIVER_STATE
/// NO_DEVICE`, gone from `Hardware` and `Capabilities`. Plugged back: it
/// runs again by itself.
pub async fn sdr_unplugged_and_back(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, _) = h.ready(cfg).await?;
        h.unplug("1-1.3")?;
        let within = cfg.hardware_within + cfg.receiver_start_within;
        let gone = |m: &[(Instant, v1::StationMessage)]| {
            !events(m, v1::event::Kind::HardwareChanged).is_empty()
                && has_state(m, 2, v1::ReceiverState::NoDevice)
        };
        let got = collect_until(&mut conn, within, within / 20, gone).await?;
        ensure(gone(&got), || {
            format!(
                "no HARDWARE_CHANGED and NO_DEVICE for receiver 2 within {within:?} \
                 of unplugging its SDR (states: {:?})",
                receiver_states(&got)
            )
        })?;
        let hw = get_hardware(&mut conn, cfg).await?;
        ensure(hw.sdrs.iter().all(|s| s.serial != "CH000002"), || {
            "the unplugged SDR is still in Hardware".into()
        })?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(receiver_ids(&caps) == [1], || {
            format!(
                "Capabilities.receivers {:?} without receiver 2's SDR",
                receiver_ids(&caps)
            )
        })?;
        h.plug(&SimulatedSdr::blog_v4("1-1.3", "CH000002"))?;
        let back = |m: &[(Instant, v1::StationMessage)]| {
            !events(m, v1::event::Kind::HardwareChanged).is_empty()
                && has_state(m, 2, v1::ReceiverState::Running)
        };
        let got = collect_until(&mut conn, within, within / 20, back).await?;
        ensure(back(&got), || {
            format!(
                "receiver 2 not RUNNING within {within:?} of its SDR's return (states: {:?})",
                receiver_states(&got)
            )
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// The applied set outlives a connection and the station process: on the
/// next connection, and after a restart, the receivers run before the hub
/// sends a set; the same set then restarts nothing.
pub async fn receivers_survive_reconnect_and_restart(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, sut| {
        let (conn, _) = h.ready(cfg).await?;
        conn.kill();
        drop(conn);
        let mut conn = h.connect(cfg.reconnect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(receiver_ids(&caps) == [1, 2], || {
            format!(
                "after a reconnect, before any set: receivers {:?}",
                receiver_ids(&caps)
            )
        })?;
        let c = configured(&configure(&mut conn, standard_set(), cfg).await?)?;
        ensure(
            c.receivers
                .iter()
                .all(|r| r.state == v1::ReceiverState::Running as i32),
            || format!("the kept set again answered {:?}", c.receivers),
        )?;
        let got = conn.collect(cfg.quiet_period).await?;
        ensure(
            events(&got, v1::event::Kind::BackendRestarted).is_empty(),
            || "the kept set restarted a receiver".into(),
        )?;
        let exit = stop_draining(sut, Some(Box::new(conn)), cfg).await?;
        ensure(exit.success, || format!("unclean exit: {}", exit.detail))?;
        sut.start(&h.target).await.map_err(station_err)?;
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = poll_capabilities(&mut conn, cfg, cfg.receiver_start_within, |c| {
            receiver_ids(c) == [1, 2]
        })
        .await?;
        ensure(receiver_ids(&caps) == [1, 2], || {
            format!(
                "after a restart, before any set: receivers {:?}",
                receiver_ids(&caps)
            )
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

// ------------------------------------------------------------------ measurements (§4.1)

/// One band in the lower half of `receiver_id`'s tunable range, clear of
/// the edges and the centre.
fn band(
    caps: &v1::Capabilities,
    receiver_id: u32,
    band_id: u32,
    key: &str,
) -> Option<v1::NoiseBand> {
    let range = receiver_caps(caps, receiver_id)?.tunable_range?;
    let width = range.end_hz.checked_sub(range.start_hz)?;
    (width >= 1_000).then(|| v1::NoiseBand {
        band_id,
        receiver_id,
        ranges: vec![v1::FrequencyRange {
            start_hz: range.start_hz + width / 10,
            end_hz: range.start_hz + width * 4 / 10,
        }],
        key: key.to_owned(),
    })
}

/// A plan with one band on each standard receiver: band 1 on receiver 1,
/// band 2 on receiver 2.
fn plan_command(caps: &v1::Capabilities, measurement_id: u32) -> Option<Command> {
    Some(Command::StartMeasurements(v1::StartMeasurements {
        measurement_id,
        noise_bands: vec![band(caps, 1, 1, "a")?, band(caps, 2, 2, "b")?],
        noise_interval_ms: limits::NOISE_INTERVAL_MS,
        bin_count: limits::MEASURE_BIN_COUNT,
        resolution_bw_hz: limits::MEASURE_RESOLUTION_BW_HZ,
        dwell_ms: limits::MEASURE_DWELL_MS,
        poll_interval_ms: limits::MEASURE_POLL_INTERVAL_MS,
    }))
}

fn noise_reports(msgs: &[(Instant, v1::StationMessage)]) -> Vec<v1::NoiseFloorReport> {
    msgs.iter()
        .filter_map(|(_, m)| match &m.body {
            Some(Body::Noise(n)) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

fn started(r: &v1::Response) -> Result<v1::MeasurementsStarted> {
    expect_code(r, v1::ResultCode::Ok, "StartMeasurements")?;
    match &r.payload {
        Some(Payload::MeasurementsStarted(m)) => Ok(m.clone()),
        other => Err(ConformanceError::Unexpected {
            expected: "MeasurementsStarted payload".into(),
            got: format!("{other:?}"),
        }),
    }
}

/// No report the hub's validator had to drop as outside the idle schedule.
fn no_unscheduled(conn: &StationConn) -> Result<()> {
    let n = conn
        .drops()
        .iter()
        .filter(|d| matches!(d, DropReason::NotScheduled))
        .count();
    ensure(n == 0, || {
        format!("{n} report(s) for minutes the idle schedule skips")
    })
}

/// Collects until `pred` holds for what arrived, or `within` passes.
async fn collect_until(
    conn: &mut StationConn,
    within: Duration,
    step: Duration,
    pred: impl Fn(&[(Instant, v1::StationMessage)]) -> bool,
) -> Result<Vec<(Instant, v1::StationMessage)>> {
    let deadline = Instant::now() + within;
    let step = step.max(Duration::from_millis(10));
    let mut all = Vec::new();
    while Instant::now() < deadline {
        all.extend(conn.collect(step).await?);
        if pred(&all) {
            break;
        }
    }
    Ok(all)
}

/// The station's capabilities and a ready connection, or `None` when it
/// doesn't measure (the scenario then passes trivially: the hub sends a
/// station without `Capabilities.measurements` no plan).
async fn measuring(
    h: &mut Harness,
    cfg: &ScenarioConfig,
) -> Result<(StationConn, Option<v1::Capabilities>)> {
    let (conn, caps) = h.ready(cfg).await?;
    let measures = caps.measurements.is_some();
    Ok((conn, measures.then_some(caps)))
}

/// `StartMeasurements` over two receivers → `MeasurementsStarted` with both
/// (checked against the request by the validator), each idle; reports then
/// arrive for both receivers, only for measuring minutes, each on a whole
/// minute and naming only that receiver's band.
pub async fn measures_when_asked(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = measuring(h, cfg).await?;
        let (Some(caps), Some(minute)) = (caps, cfg.measure_minute) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let Some(plan) = plan_command(&caps, 5) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let s = started(&conn.call(plan, cfg.response_within).await?)?;
        let mut measured: Vec<u32> = s.receivers.iter().map(|r| r.receiver_id).collect();
        measured.sort_unstable();
        ensure(measured == [1, 2], || {
            format!("MeasurementsStarted for receivers {measured:?}, expected [1, 2]")
        })?;
        for r in &s.receivers {
            ensure(r.duty == v1::MeasurementDuty::Idle as i32, || {
                format!(
                    "receiver {} without listeners must start idle, got duty {}",
                    r.receiver_id, r.duty
                )
            })?;
        }
        // Within 6 minutes a measuring minute has passed and been reported,
        // for each receiver.
        let both = |m: &[(Instant, v1::StationMessage)]| {
            let rx: HashSet<u32> = noise_reports(m).iter().map(|n| n.receiver_id).collect();
            rx.len() == 2
        };
        let got = collect_until(&mut conn, minute * 7, minute / 2, both).await?;
        ensure(both(&got), || {
            "no NoiseFloorReport for each receiver within 7 minutes of idle duty".into()
        })?;
        for n in noise_reports(&got) {
            ensure(n.measurement_id == 5, || "a report for another plan".into())?;
            ensure(n.window_start_unix_ms.rem_euclid(60_000) == 0, || {
                format!("window {} is not a UTC minute", n.window_start_unix_ms)
            })?;
            ensure(n.bands.iter().all(|b| b.band_id == n.receiver_id), || {
                format!(
                    "receiver {}'s report names another receiver's band",
                    n.receiver_id
                )
            })?;
        }
        no_unscheduled(&conn)?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

fn duty_events(
    msgs: &[(Instant, v1::StationMessage)],
    receiver_id: u32,
    duty: v1::MeasurementDuty,
) -> Vec<v1::Event> {
    events(msgs, v1::event::Kind::MeasurementDuty)
        .into_iter()
        .filter(|e| e.receiver_id == Some(receiver_id) && e.measurement_duty == Some(duty as i32))
        .collect()
}

/// A listener channel on receiver 1 makes its duty continuous from a minute
/// boundary (`Event{MEASUREMENT_DUTY}` for receiver 1), with a report every
/// minute, while receiver 2 stays idle; closing it makes receiver 1 idle
/// again.
pub async fn duty_follows_listeners(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = measuring(h, cfg).await?;
        let (Some(caps), Some(minute)) = (caps, cfg.measure_minute) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let (Some(plan), Some(open_1)) = (plan_command(&caps, 6), open_command(&caps, 1, 1)) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        started(&conn.call(plan, cfg.response_within).await?)?;
        open(&mut conn, open_1, cfg).await?;
        let continuous = |m: &[(Instant, v1::StationMessage)]| {
            !duty_events(m, 1, v1::MeasurementDuty::Continuous).is_empty()
        };
        let mut got = collect_until(&mut conn, minute * 2, minute / 4, continuous).await?;
        let change = duty_events(&got, 1, v1::MeasurementDuty::Continuous);
        let Some(change) = change.first() else {
            return Err(ConformanceError::Unexpected {
                expected: "Event{MEASUREMENT_DUTY} continuous for receiver 1 within 2 minutes \
                           of a listener"
                    .into(),
                got: format!("{:?}", events(&got, v1::event::Kind::MeasurementDuty)),
            });
        };
        let from = change.effective_unix_ms.unwrap_or(1);
        ensure(from.rem_euclid(60_000) == 0, || {
            format!("duty from {from}: not a minute")
        })?;
        // Continuous: two consecutive minutes of receiver 1 are reported.
        let two = |m: &[(Instant, v1::StationMessage)]| {
            let starts: Vec<i64> = noise_reports(m)
                .iter()
                .filter(|n| n.receiver_id == 1)
                .map(|n| n.window_start_unix_ms)
                .collect();
            starts.windows(2).any(|w| w[1] - w[0] == 60_000)
        };
        let more = collect_until(&mut conn, minute * 3, minute / 2, two).await?;
        ensure(two(&more), || {
            "no consecutive minutes of receiver 1 reported under continuous duty".into()
        })?;
        got.extend(more);
        ensure(
            duty_events(&got, 2, v1::MeasurementDuty::Continuous).is_empty(),
            || "receiver 2 went continuous with a listener on receiver 1 only".into(),
        )?;
        let close = Command::CloseChannel(v1::CloseChannel { channel_id: 1 });
        expect_code(
            &conn.call(close, cfg.response_within).await?,
            v1::ResultCode::Ok,
            "CloseChannel",
        )?;
        let idle = |m: &[(Instant, v1::StationMessage)]| {
            !duty_events(m, 1, v1::MeasurementDuty::Idle).is_empty()
        };
        let got = collect_until(&mut conn, minute * 2, minute / 4, idle).await?;
        ensure(idle(&got), || {
            "receiver 1 not idle within 2 minutes of its last listener leaving".into()
        })?;
        no_unscheduled(&conn)?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `SetFrontend` on receiver 1 stops the running plan, for every receiver:
/// one `Event{MEASUREMENT_STOPPED}` with the front-end reason, and a new plan
/// may start right away.
pub async fn stops_measuring_on_frontend_change(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = measuring(h, cfg).await?;
        let Some(caps) = caps else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let can_retune = receiver_caps(&caps, 1)
            .and_then(|r| r.frontend.as_ref())
            .is_some_and(|f| f.can_retune);
        let Some(plan) = plan_command(&caps, 7).filter(|_| can_retune) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        started(&conn.call(plan, cfg.response_within).await?)?;
        let set = Command::SetFrontend(v1::SetFrontend {
            receiver_id: 1,
            center_frequency_hz: Some(7_200_000),
            ..Default::default()
        });
        expect_code(
            &conn.call(set, cfg.response_within).await?,
            v1::ResultCode::Ok,
            "SetFrontend",
        )?;
        let got = conn.collect(cfg.quiet_period).await?;
        let stopped = events(&got, v1::event::Kind::MeasurementStopped);
        ensure(stopped.len() == 1, || {
            format!(
                "expected one Event{{MEASUREMENT_STOPPED}}, got {}",
                stopped.len()
            )
        })?;
        ensure(stopped[0].measurement_id == Some(7), || {
            "MEASUREMENT_STOPPED for another plan".into()
        })?;
        ensure(
            stopped[0].measurement_stop_reason
                == Some(v1::MeasurementStopReason::FrontendChanged as i32),
            || format!("stop reason {:?}", stopped[0].measurement_stop_reason),
        )?;
        // A new plan may start right away, on the new passband.
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        let Some(again) = plan_command(&caps, 8) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        started(&conn.call(again, cfg.response_within).await?)?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// One plan per connection: a second `StartMeasurements` is `ALREADY_EXISTS`;
/// `StopMeasurements` stops it (an unknown plan is `NOT_FOUND`); a plan does
/// not survive a reconnect (a report for it then would be a violation).
pub async fn one_plan_per_connection(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new().await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let (mut conn, caps) = measuring(h, cfg).await?;
        let Some(caps) = caps else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let (Some(first), Some(second)) = (plan_command(&caps, 1), plan_command(&caps, 2)) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        started(&conn.call(first.clone(), cfg.response_within).await?)?;
        expect_code(
            &conn.call(second, cfg.response_within).await?,
            v1::ResultCode::AlreadyExists,
            "a second StartMeasurements",
        )?;
        let stop =
            |measurement_id| Command::StopMeasurements(v1::StopMeasurements { measurement_id });
        expect_code(
            &conn.call(stop(9), cfg.response_within).await?,
            v1::ResultCode::NotFound,
            "StopMeasurements of an unknown plan",
        )?;
        expect_code(
            &conn.call(stop(1), cfg.response_within).await?,
            v1::ResultCode::Ok,
            "StopMeasurements",
        )?;
        started(&conn.call(first, cfg.response_within).await?)?;
        conn.kill();
        drop(conn);
        let mut conn = h.connect(cfg.reconnect_within, cfg).await?;
        // Reports of the old plan now would be a violation (never started here).
        let window = cfg.measure_minute.map_or(cfg.quiet_period, |m| m * 6);
        let got = conn.collect(window).await?;
        ensure(noise_reports(&got).is_empty(), || {
            "reports of the previous connection's plan".into()
        })?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}
