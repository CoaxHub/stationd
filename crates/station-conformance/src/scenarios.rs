// SPDX-License-Identifier: MIT
//! Named conformance scenarios. Each starts a fresh [`FakeHub`] and test
//! PKI, starts the station under test against it, scripts the hub side,
//! and finally stops the station, which must exit cleanly (unless the
//! scenario says otherwise).
//!
//! Every station message in every scenario also passes the hub-side §5
//! validator, so e.g. an unsolicited or duplicate response, audio for a
//! channel the hub never opened, or a malformed `Capabilities` fails any
//! scenario.
//!
//! Timing expectations come from [`ScenarioConfig`]: the defaults fit the
//! production timings of station-protocol §2.1/§2.2 (slow: backoff up to
//! 60 s); [`ScenarioConfig::fast`] fits a station configured with short
//! test intervals.

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
use crate::sut::{LocalPolicy, StationUnderTest, Target};

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
        }
    }
}

impl ScenarioConfig {
    /// For a station configured with test timings: backoff at most ~300 ms,
    /// HTTP/2 keepalive ~300 ms.
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
        "Capabilities valid and limited by the local policy (§4, §7)"
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
        admin_op_denied_by_default,
        "every AdminOp is PERMISSION_DENIED with the default policy (§4, §7)"
    ),
    scenario!(
        admin_op_allowed_when_configured,
        "an allowed AdminOp gets OK, others are denied (§4)"
    ),
    scenario!(
        set_frontend_denied_when_disabled,
        "SetFrontend is PERMISSION_DENIED when frontend control is off (§4)"
    ),
    scenario!(
        status_at_interval,
        "StationStatus every Welcome.status_interval_ms (§2, §5)"
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
        "OpenChannel → audio for that channel; CloseChannel stops it (§4, §6.1)"
    ),
    scenario!(
        subscribes_waterfall_and_streams_rows,
        "SubscribeWaterfall → valid rows; UnsubscribeWaterfall stops them (§4, §6.2)"
    ),
    scenario!(
        drops_channels_on_disconnect,
        "channels do not survive a reconnect (§2.3)"
    ),
];

/// The scenario called `name`.
pub fn by_name(name: &str) -> Option<&'static Scenario> {
    ALL.iter().find(|s| s.name == name)
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
    /// A harness whose hub uses the PKI's own certificate (TLS 1.3).
    pub async fn new(policy: LocalPolicy) -> Result<Self> {
        Self::with_hub_tls(policy, TestPki::hub_tls).await
    }

    /// A harness whose hub TLS comes from `hub_tls` (the station always
    /// trusts only `pki.ca_pem`).
    pub async fn with_hub_tls(
        policy: LocalPolicy,
        hub_tls: impl FnOnce(&TestPki) -> HubTls,
    ) -> Result<Self> {
        let pki = TestPki::generate()?;
        let hub = FakeHub::start(&hub_tls(&pki)).await?;
        let dir = tempfile::tempdir().map_err(io_err)?;
        let path = |name: &str| dir.path().join(name);
        std::fs::write(path("gateway-ca.pem"), &pki.ca_pem).map_err(io_err)?;
        std::fs::write(path("station.pem"), &pki.station_cert_pem).map_err(io_err)?;
        write_private(&path("station.key"), &pki.station_key_pem)?;
        let work_dir = path("work");
        std::fs::create_dir(&work_dir).map_err(io_err)?;
        let target = Target {
            gateway_url: hub.gateway_url(),
            gateway_ca: path("gateway-ca.pem"),
            cert: path("station.pem"),
            key: path("station.key"),
            station_id: pki.station_id,
            policy,
            work_dir,
        };
        Ok(Self {
            hub,
            pki,
            target,
            _dir: dir,
        })
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
            let drain = async {
                if let Some(mut c) = conn {
                    let _ = c.expect_closed(cfg.exit_within).await;
                }
            };
            let (exit, ()) = tokio::join!(sut.stop(), drain);
            let exit = exit.map_err(station_err)?;
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

fn admin_op(kind: v1::AdminOpKind) -> Command {
    Command::AdminOp(v1::AdminOp { kind: kind as i32 })
}

const ALL_ADMIN_OPS: [v1::AdminOpKind; 4] = [
    v1::AdminOpKind::RestartStationd,
    v1::AdminOpKind::RestartBackend,
    v1::AdminOpKind::Update,
    v1::AdminOpKind::RebootHost,
];

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

/// Hello comes first, with sane fields; the station sends nothing else
/// before `Welcome`; the session then works.
pub async fn hello_then_welcome(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new(LocalPolicy::default()).await;
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
                    "Hello.protocol_version.major is {}, expected 1",
                    version.major
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

/// `GetCapabilities` answers valid capabilities that reflect the local
/// policy (§4 table, §5 checks, §7 caps).
pub async fn answers_get_capabilities(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let policy = LocalPolicy {
        allow_frontend_control: false,
        allowed_admin_ops: vec![v1::AdminOpKind::RestartBackend],
        max_channels: 2,
        max_waterfall_subscriptions: 1,
    };
    let harness = Harness::new(policy.clone()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        let mut ops = caps.allowed_admin_ops.clone();
        ops.sort_unstable();
        let expected: Vec<i32> = policy.allowed_admin_ops.iter().map(|k| *k as i32).collect();
        ensure(ops == expected, || {
            format!("allowed_admin_ops {ops:?}, local policy {expected:?}")
        })?;
        ensure(!caps.frontend_control_allowed, || {
            "frontend_control_allowed is true, local policy says false".into()
        })?;
        ensure(caps.max_channels <= policy.max_channels, || {
            format!(
                "max_channels {} above the local cap {}",
                caps.max_channels, policy.max_channels
            )
        })?;
        if let Some(w) = caps.waterfall {
            ensure(
                w.max_subscriptions <= policy.max_waterfall_subscriptions,
                || {
                    format!(
                        "waterfall.max_subscriptions {} above the local cap",
                        w.max_subscriptions
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
    let harness = Harness::new(LocalPolicy::default()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let mut ids = Vec::new();
        for i in 0..6u32 {
            ids.push(conn.request(get_capabilities()).await?);
            ids.push(conn.request(ping(u64::from(i))).await?);
            ids.push(conn.request(admin_op(v1::AdminOpKind::Update)).await?);
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
            conn.request(Command::SetFrontend(v1::SetFrontend::default()))
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
                Command::AdminOp(v1::AdminOp { kind: 99 }),
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
        expect_pong(&mut conn, 5, cfg).await?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// With the default policy every `AdminOp` is `PERMISSION_DENIED`.
pub async fn admin_op_denied_by_default(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new(LocalPolicy::default()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(caps.allowed_admin_ops.is_empty(), || {
            format!(
                "allowed_admin_ops should be empty, got {:?}",
                caps.allowed_admin_ops
            )
        })?;
        for kind in ALL_ADMIN_OPS {
            let r = conn.call(admin_op(kind), cfg.response_within).await?;
            expect_code(&r, v1::ResultCode::PermissionDenied, kind.as_str_name())?;
        }
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// An allowed operation (`restart_backend`) gets `OK`; the others are
/// still denied. The station may really perform it: run it with a
/// harmless backend.
pub async fn admin_op_allowed_when_configured(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let policy = LocalPolicy {
        allowed_admin_ops: vec![v1::AdminOpKind::RestartBackend],
        ..LocalPolicy::default()
    };
    let harness = Harness::new(policy).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let r = conn
            .call(admin_op(v1::AdminOpKind::Update), cfg.response_within)
            .await?;
        expect_code(
            &r,
            v1::ResultCode::PermissionDenied,
            "AdminOp UPDATE (not allowed)",
        )?;
        let r = conn
            .call(
                admin_op(v1::AdminOpKind::RestartBackend),
                cfg.response_within,
            )
            .await?;
        expect_code(&r, v1::ResultCode::Ok, "AdminOp RESTART_BACKEND (allowed)")?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `SetFrontend` is refused when the owner disabled frontend control.
pub async fn set_frontend_denied_when_disabled(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let policy = LocalPolicy {
        allow_frontend_control: false,
        ..LocalPolicy::default()
    };
    let harness = Harness::new(policy).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        ensure(!caps.frontend_control_allowed, || {
            "frontend_control_allowed should be false".into()
        })?;
        let set = Command::SetFrontend(v1::SetFrontend {
            gain_db: Some(10.0),
            ..Default::default()
        });
        let r = conn.call(set, cfg.response_within).await?;
        expect_code(&r, v1::ResultCode::PermissionDenied, "SetFrontend")?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}

/// `StationStatus` arrives about every `status_interval_ms`, never faster
/// than the §5 rate limit.
pub async fn status_at_interval(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new(LocalPolicy::default()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let interval = Duration::from_millis(u64::from(cfg.status_interval_ms));
        let window = interval * 5 + interval / 2;
        let got = conn.collect(window).await?;
        let times: Vec<Instant> = got
            .iter()
            .filter(|(_, m)| matches!(m.body, Some(Body::Status(_))))
            .map(|(t, _)| *t)
            .collect();
        ensure((4..=7).contains(&times.len()), || {
            format!(
                "{} StationStatus in {window:?} at a {interval:?} interval",
                times.len()
            )
        })?;
        for pair in times.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]);
            ensure(gap >= interval / 2, || {
                format!("two StationStatus only {gap:?} apart")
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
    let harness = Harness::new(LocalPolicy::default()).await;
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
    let harness = Harness::with_hub_tls(LocalPolicy::default(), |pki| HubTls {
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
    let harness = Harness::with_hub_tls(LocalPolicy::default(), |pki| HubTls {
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
    let harness = Harness::new(LocalPolicy::default()).await;
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

/// An `OpenChannel` inside the station's capabilities, or `None` if it has
/// no audio (the channel scenarios then pass trivially).
fn open_command(caps: &v1::Capabilities, channel_id: u32) -> Option<Command> {
    let range = caps.tunable_range?;
    let mode = *caps.modes.first()?;
    if caps.max_channels == 0 || !caps.codecs.contains(&(v1::AudioCodec::Opus as i32)) {
        return None;
    }
    Some(Command::OpenChannel(v1::OpenChannel {
        channel_id,
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

/// Audio flows for an opened channel and stops after `CloseChannel`.
pub async fn opens_channel_and_streams_audio(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new(LocalPolicy::default()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        let Some(open) = open_command(&caps, 1) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let r = conn.call(open, cfg.response_within).await?;
        expect_code(&r, v1::ResultCode::Ok, "OpenChannel")?;
        ensure(matches!(r.payload, Some(Payload::ChannelOpened(_))), || {
            "no ChannelOpened".into()
        })?;
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

/// Rows flow for a subscription (each decompresses to exactly `bins`,
/// checked by the validator) and stop after `UnsubscribeWaterfall`.
pub async fn subscribes_waterfall_and_streams_rows(
    sut: &mut dyn StationUnderTest,
    cfg: &ScenarioConfig,
) -> Result<()> {
    let harness = Harness::new(LocalPolicy::default()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        let (Some(range), Some(w)) = (caps.tunable_range, caps.waterfall) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        if w.max_subscriptions == 0 || w.max_bins < limits::MIN_BINS {
            return Ok(After::Stop(Some(Box::new(conn))));
        }
        let row_interval_ms = w.min_row_interval_ms.max(100);
        let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            subscription_id: 1,
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
    let harness = Harness::new(LocalPolicy::default()).await;
    run_scenario(sut, cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, cfg).await?;
        let caps = capabilities(&conn.call(get_capabilities(), cfg.response_within).await?)?;
        let Some(open) = open_command(&caps, 1) else {
            return Ok(After::Stop(Some(Box::new(conn))));
        };
        let r = conn.call(open.clone(), cfg.response_within).await?;
        expect_code(&r, v1::ResultCode::Ok, "OpenChannel")?;
        conn.kill();
        drop(conn);
        let mut conn = h.connect(cfg.reconnect_within, cfg).await?;
        // Audio for channel 1 now would be a violation (never opened here).
        let got = conn.collect(cfg.quiet_period).await?;
        ensure(audio_frames(&got, 1) == 0, || {
            "audio for a channel of the previous connection".into()
        })?;
        let r = conn.call(open, cfg.response_within).await?;
        expect_code(
            &r,
            v1::ResultCode::Ok,
            "OpenChannel of the same ID on the new connection",
        )?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await
}
