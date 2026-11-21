// SPDX-License-Identifier: GPL-3.0-only
//! Request handling against receivers on fake backends.

use std::time::Duration;

use futures_util::StreamExt as _;
use station_backend::{Backend, ChannelOutput, FakeBackend};
use station_proto::validate::sanitize_capabilities;

use super::*;
use crate::profile::ProfileId;
use crate::receivers::testing::{STANDARD_SDRS, Station, receiver, standard_set};
use crate::receivers::{Driver, FakeUnits, ReceiverOptions, ReceiverPaths, SharedBackend};

fn ctx_for(st: &Station) -> RequestContext {
    RequestContext {
        receivers: st.receivers.clone(),
        minor: 0,
        tables: Arc::default(),
        measurer: None,
    }
}

fn req(id: u64, command: Command) -> v1::Request {
    v1::Request {
        request_id: id,
        command: Some(command),
    }
}

fn code(r: &v1::Response) -> v1::ResultCode {
    r.result
        .as_ref()
        .and_then(|r| v1::ResultCode::try_from(r.code).ok())
        .unwrap_or(v1::ResultCode::Unspecified)
}

fn open(channel_id: u32, receiver_id: u32, frequency_hz: u64) -> Command {
    Command::OpenChannel(v1::OpenChannel {
        channel_id,
        receiver_id,
        frequency_hz,
        mode: v1::Mode::Usb as i32,
        low_cut_hz: 200,
        high_cut_hz: 2800,
        audio: Some(v1::AudioParams {
            codec: v1::AudioCodec::Opus as i32,
            bitrate_bps: 16_000,
            sample_rate_hz: 12_000,
            frame_duration_ms: 20,
        }),
        squelch_snr_db: None,
    })
}

fn subscribe_cmd(subscription_id: u32, receiver_id: u32, center_hz: u64) -> Command {
    Command::SubscribeWaterfall(v1::SubscribeWaterfall {
        subscription_id,
        receiver_id,
        range: Some(v1::FrequencyRange {
            start_hz: center_hz - 100_000,
            end_hz: center_hz + 100_000,
        }),
        bins: 128,
        row_interval_ms: 100,
        quantization: None,
        compression: v1::WaterfallCompression::Zstd as i32,
    })
}

#[test]
fn truncation_respects_char_boundaries() {
    assert_eq!(truncate_utf8("abc", 5), "abc");
    assert_eq!(truncate_utf8("abcdef", 3), "abc");
    assert_eq!(truncate_utf8("aé", 2), "a");
}

#[tokio::test]
async fn dispatch_table() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let (r, _) = dispatch(&c, req(1, Command::GetCapabilities(v1::GetCapabilities {}))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let Some(Payload::Capabilities(caps)) = r.payload else {
        panic!("capabilities expected")
    };
    assert_eq!(caps.receivers.len(), 2);
    assert_eq!(caps.measurements, None, "no measurer on this connection");
    assert!(sanitize_capabilities(&caps, 0).is_ok());
    let (r, _) = dispatch(&c, req(2, Command::GetHardware(v1::GetHardware {}))).await;
    let Some(Payload::Hardware(hw)) = r.payload else {
        panic!("hardware expected")
    };
    assert_eq!(hw.sdrs.len(), 2);
    let (r, _) = dispatch(
        &c,
        req(
            3,
            Command::Ping(v1::Ping {
                nonce: 77,
                hub_time_unix_ms: 0,
            }),
        ),
    )
    .await;
    assert_eq!(r.request_id, 3);
    let Some(Payload::Pong(pong)) = r.payload else {
        panic!("pong expected")
    };
    assert_eq!(pong.nonce, 77);
    assert!(pong.station_time_unix_ms > 0);
    // §3: unknown command, unknown enum value, missing field, id 0.
    let (r, _) = dispatch(
        &c,
        v1::Request {
            request_id: 4,
            command: None,
        },
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Unsupported);
    let admin = |kind| {
        Command::AdminOp(v1::AdminOp {
            kind,
            receiver_id: None,
        })
    };
    let (r, _) = dispatch(&c, req(5, admin(99))).await;
    assert_eq!(code(&r), v1::ResultCode::Unsupported);
    let (r, _) = dispatch(&c, req(6, admin(0))).await;
    assert_eq!(code(&r), v1::ResultCode::InvalidArgument);
    let (r, _) = dispatch(&c, req(0, Command::Ping(v1::Ping::default()))).await;
    assert_eq!(code(&r), v1::ResultCode::InvalidArgument);
    // No admin operation is implemented.
    for kind in [
        v1::AdminOpKind::RestartStationd,
        v1::AdminOpKind::RestartBackend,
        v1::AdminOpKind::Update,
        v1::AdminOpKind::RebootHost,
    ] {
        let (r, _) = dispatch(&c, req(7, admin(kind as i32))).await;
        assert_eq!(code(&r), v1::ResultCode::Unsupported, "{kind:?}");
        assert!(r.payload.is_none());
    }
}

#[tokio::test]
async fn unknown_ids_and_no_measurer() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    for (id, cmd) in [
        (8, Command::CloseChannel(v1::CloseChannel { channel_id: 9 })),
        (
            9,
            Command::UpdateChannel(v1::UpdateChannel {
                channel_id: 9,
                ..Default::default()
            }),
        ),
        (
            10,
            Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 9 }),
        ),
        (11, open(1, 9, 7_100_000)),
        (12, subscribe_cmd(1, 9, 7_100_000)),
        (
            13,
            Command::SetFrontend(v1::SetFrontend {
                receiver_id: 9,
                ..Default::default()
            }),
        ),
    ] {
        let (r, m) = dispatch(&c, req(id, cmd)).await;
        assert_eq!(code(&r), v1::ResultCode::NotFound, "{id}");
        assert_eq!(r.request_id, id);
        assert!(m.is_none());
    }
    // Measuring without a measurer.
    let (r, _) = dispatch(
        &c,
        req(
            14,
            Command::StopMeasurements(v1::StopMeasurements { measurement_id: 1 }),
        ),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::NotFound);
    let (r, _) = dispatch(
        &c,
        req(
            15,
            Command::StartMeasurements(v1::StartMeasurements {
                measurement_id: 1,
                ..Default::default()
            }),
        ),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Unsupported);
}

#[tokio::test]
async fn receiver_sets() {
    let st = Station::new(&STANDARD_SDRS).await;
    let c = ctx_for(&st);
    let (r, _) = dispatch(&c, req(1, Command::ConfigureReceivers(standard_set()))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let Some(Payload::ReceiversConfigured(answer)) = r.payload else {
        panic!("receivers configured expected")
    };
    assert_eq!(answer.receivers.len(), 2);
    let mut overlap = standard_set();
    overlap.receivers[1].center_frequency_hz = 7_500_000;
    let (r, _) = dispatch(&c, req(2, Command::ConfigureReceivers(overlap))).await;
    assert_eq!(code(&r), v1::ResultCode::InvalidArgument);
    assert_eq!(
        r.result.map(|r| r.message).as_deref(),
        Some("receiver 2: tunable range overlaps receiver 1's")
    );
    assert!(r.payload.is_none());
    let mut unknown = standard_set();
    unknown.receivers[0].model = 77;
    let (r, _) = dispatch(&c, req(3, Command::ConfigureReceivers(unknown))).await;
    assert_eq!(code(&r), v1::ResultCode::Unsupported);
}

#[tokio::test]
async fn set_frontend_on_a_receiver() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let set = |f: v1::SetFrontend| Command::SetFrontend(f);
    // The fake's receivers retune and nothing else, as radiod's.
    let (r, _) = dispatch(
        &c,
        req(
            1,
            set(v1::SetFrontend {
                receiver_id: 1,
                gain_db: Some(10.0),
                ..Default::default()
            }),
        ),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Unsupported);
    let (r, _) = dispatch(
        &c,
        req(
            2,
            set(v1::SetFrontend {
                receiver_id: 1,
                center_frequency_hz: Some(7_200_000),
                ..Default::default()
            }),
        ),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let Some(Payload::FrontendState(state)) = r.payload else {
        panic!("frontend state expected")
    };
    assert_eq!(state.receiver_id, 1);
    assert_eq!(state.center_frequency_hz, 7_200_000);
    assert_eq!(
        st.receivers.config(1).map(|c| c.center_frequency_hz),
        Some(7_200_000),
        "kept in receivers.json"
    );
    for (center, want) in [
        (40_000_000, v1::ResultCode::OutOfRange),
        (13_500_000, v1::ResultCode::InvalidArgument),
    ] {
        let (r, _) = dispatch(
            &c,
            req(
                3,
                set(v1::SetFrontend {
                    receiver_id: 1,
                    center_frequency_hz: Some(center),
                    ..Default::default()
                }),
            ),
        )
        .await;
        assert_eq!(code(&r), want, "{center}");
    }
}

#[tokio::test]
async fn channels_live_on_their_receiver() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let (r, media) = dispatch(&c, req(1, open(5, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let Some(Payload::ChannelOpened(opened)) = &r.payload else {
        panic!("opened expected")
    };
    assert_eq!((opened.channel_id, opened.receiver_id), (5, 1));
    let Some(Media::Audio {
        channel_id,
        mut stream,
        stop,
    }) = media
    else {
        panic!("audio expected")
    };
    assert_eq!(channel_id, 5);
    assert!(matches!(stream.next().await, Some(ChannelOutput::Audio(_))));
    assert_eq!(c.tables().open_channels(), 1);
    assert_eq!(c.tables().open_channels_on(1), 1);
    assert_eq!(c.tables().listeners_on(2), 0);
    let (r, _) = dispatch(&c, req(2, open(5, 2, 14_200_000))).await;
    assert_eq!(code(&r), v1::ResultCode::AlreadyExists);
    // Outside its receiver's passband (it's receiver 2's).
    let (r, _) = dispatch(&c, req(3, open(6, 1, 14_200_000))).await;
    assert_eq!(code(&r), v1::ResultCode::OutOfRange);
    assert!(!c.tables().channels.contains_key(&6));
    let upd = Command::UpdateChannel(v1::UpdateChannel {
        channel_id: 5,
        frequency_hz: Some(7_050_000),
        ..Default::default()
    });
    let (r, _) = dispatch(&c, req(4, upd)).await;
    assert!(
        matches!(r.payload, Some(Payload::ChannelUpdated(ref u)) if u.frequency_hz == 7_050_000)
    );
    let (r, _) = dispatch(
        &c,
        req(5, Command::CloseChannel(v1::CloseChannel { channel_id: 5 })),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    assert!(stop.is_cancelled());
    assert!(
        st.backends
            .get(1)
            .expect("backend")
            .channel_ids()
            .is_empty()
    );
    assert_eq!(c.tables().open_channels(), 0);
}

#[tokio::test]
async fn a_receiver_has_sixteen_channels() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    for id in 1..=limits::CHANNELS_PER_RECEIVER {
        let (r, _) = dispatch(&c, req(u64::from(id), open(id, 1, 7_100_000))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok, "{id}");
    }
    let (r, _) = dispatch(&c, req(100, open(100, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::ResourceExhausted);
    assert!(
        r.result
            .is_some_and(|r| r.message == "receiver 1 has all its 16 channels in use")
    );
    let (r, _) = dispatch(&c, req(101, open(101, 2, 14_200_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
}

#[tokio::test]
async fn waterfall_subscriptions_across_receivers() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let claim = limits::waterfall_subscriptions_claim(2);
    let mut id = 0;
    for (rx, center) in [(1, 7_100_000), (2, 14_200_000)] {
        for _ in 0..claim / 2 {
            id += 1;
            let (r, media) = dispatch(&c, req(u64::from(id), subscribe_cmd(id, rx, center))).await;
            assert_eq!(code(&r), v1::ResultCode::Ok, "{id}");
            let Some(Payload::WaterfallSubscribed(s)) = &r.payload else {
                panic!("subscribed expected")
            };
            assert_eq!(s.receiver_id, rx);
            assert!(matches!(media, Some(Media::Rows { .. })));
        }
    }
    let (r, _) = dispatch(&c, req(50, subscribe_cmd(50, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::ResourceExhausted);
    let (r, _) = dispatch(&c, req(51, subscribe_cmd(1, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::AlreadyExists);
    let (r, _) = dispatch(
        &c,
        req(
            52,
            Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 1 }),
        ),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    // A backend refusal frees the slot.
    let mut bad = subscribe_cmd(53, 1, 7_100_000);
    if let Command::SubscribeWaterfall(s) = &mut bad {
        s.bins = 1;
    }
    let (r, _) = dispatch(&c, req(53, bad)).await;
    assert_eq!(code(&r), v1::ResultCode::OutOfRange);
    assert!(!c.tables().subscriptions.contains_key(&53));
}

#[tokio::test]
async fn a_receiver_going_takes_its_channels() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let (_, m1) = dispatch(&c, req(1, open(1, 1, 7_100_000))).await;
    let (_, m2) = dispatch(&c, req(2, open(2, 2, 14_200_000))).await;
    let (_, m3) = dispatch(&c, req(3, subscribe_cmd(3, 2, 14_200_000))).await;
    c.tables().receiver_gone(2);
    assert_eq!(c.tables().open_channels(), 1);
    assert_eq!(c.tables().open_subscriptions(), 0);
    for (m, gone) in [(m1, false), (m2, true), (m3, true)] {
        match m.expect("media") {
            Media::Audio { stop, .. } | Media::Rows { stop, .. } => {
                assert_eq!(stop.is_cancelled(), gone);
            }
        }
    }
    // Closing a channel whose receiver went answers OK.
    st.receivers
        .configure(&v1::ConfigureReceivers {
            receivers: vec![receiver(2, "CH000002", ProfileId::RtlsdrV4, 14_200_000)],
        })
        .await
        .expect("set");
    let (r, _) = dispatch(
        &c,
        req(4, Command::CloseChannel(v1::CloseChannel { channel_id: 1 })),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let (_, m) = dispatch(&c, req(5, open(4, 2, 14_200_000))).await;
    c.tables().channel_closed(4);
    assert!(matches!(m, Some(Media::Audio { ref stop, .. }) if stop.is_cancelled()));
    let (_, m) = dispatch(&c, req(6, subscribe_cmd(5, 2, 14_200_000))).await;
    c.tables().subscription_closed(5);
    assert!(matches!(m, Some(Media::Rows { ref stop, .. }) if stop.is_cancelled()));
    let (_, _m) = dispatch(&c, req(7, open(6, 2, 14_200_000))).await;
    c.tables().channels.insert(7, Entry::opening(2));
    let (channels, subs) = c.tables().take_all();
    assert_eq!(channels, vec![(2, 6), (2, 7)]);
    assert!(subs.is_empty());
    close_in_backends(&st.receivers, channels, subs).await;
    assert!(
        !st.backends
            .get(2)
            .expect("backend")
            .channel_ids()
            .contains(&6)
    );
    assert!(format!("{c:?}").contains("RequestContext"));
}

/// A backend whose `open_channel`/`subscribe_waterfall` wait for a gate, and
/// whose closes wait for another (open by default).
struct Gated {
    inner: FakeBackend,
    gate: Arc<tokio::sync::Semaphore>,
    close_gate: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl Backend for Gated {
    fn info(&self) -> v1::BackendInfo {
        self.inner.info()
    }
    async fn capabilities(&self) -> Result<station_backend::BackendCapabilities, BackendError> {
        self.inner.capabilities().await
    }
    async fn open_channel(
        &self,
        req: v1::OpenChannel,
    ) -> Result<(v1::ChannelOpened, AudioStream), BackendError> {
        let _permit = self.gate.acquire().await;
        self.inner.open_channel(req).await
    }
    async fn update_channel(
        &self,
        req: v1::UpdateChannel,
    ) -> Result<v1::ChannelUpdated, BackendError> {
        self.inner.update_channel(req).await
    }
    async fn close_channel(&self, id: u32) -> Result<(), BackendError> {
        let _permit = self.close_gate.acquire().await;
        self.inner.close_channel(id).await
    }
    async fn set_frontend(&self, req: v1::SetFrontend) -> Result<v1::FrontendState, BackendError> {
        self.inner.set_frontend(req).await
    }
    async fn subscribe_waterfall(
        &self,
        req: v1::SubscribeWaterfall,
    ) -> Result<(v1::WaterfallSubscribed, RowStream), BackendError> {
        let _permit = self.gate.acquire().await;
        self.inner.subscribe_waterfall(req).await
    }
    async fn unsubscribe_waterfall(&self, id: u32) -> Result<(), BackendError> {
        let _permit = self.close_gate.acquire().await;
        self.inner.unsubscribe_waterfall(id).await
    }
    fn health(&self) -> station_backend::Health {
        self.inner.health()
    }
    fn events(&self) -> station_backend::EventStream {
        self.inner.events()
    }
}

/// A running receiver on a [`Gated`] backend.
struct Rig {
    c: RequestContext,
    inner: FakeBackend,
    /// Opens wait for its permits (none at first).
    gate: Arc<tokio::sync::Semaphore>,
    /// Closes wait for its permits (plenty at first).
    close_gate: Arc<tokio::sync::Semaphore>,
    dir: tempfile::TempDir,
}

/// One running receiver (1) on a backend whose opens wait for `gate`
/// (a slow open): the context, the fake underneath and the gate.
async fn gated() -> (
    RequestContext,
    FakeBackend,
    Arc<tokio::sync::Semaphore>,
    tempfile::TempDir,
) {
    let r = rig().await;
    (r.c, r.inner, r.gate, r.dir)
}

async fn rig() -> Rig {
    let inner = FakeBackend::default();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let close_gate = Arc::new(tokio::sync::Semaphore::new(1 << 20));
    let dir = tempfile::tempdir().expect("tmp");
    crate::receivers::testing::plug(&dir.path().join("usb"), "1-1", "S", "RTL2838UHIDIR");
    let receivers = crate::receivers::Receivers::new(
        ReceiverPaths {
            store: dir.path().join("receivers.json"),
            usb_devices: dir.path().join("usb"),
            packaged: false,
        },
        Driver {
            backends: Arc::new(SharedBackend(Arc::new(Gated {
                inner: inner.clone(),
                gate: Arc::clone(&gate),
                close_gate: Arc::clone(&close_gate),
            }))),
            units: Arc::new(FakeUnits::default()),
        },
        ReceiverOptions {
            poll: Duration::from_millis(5),
            hardware_poll: Duration::from_millis(5),
        },
    );
    receivers.start().await;
    receivers
        .configure(&v1::ConfigureReceivers {
            receivers: vec![receiver(1, "S", ProfileId::RtlsdrHf, 7_100_000)],
        })
        .await
        .expect("set");
    while receivers.running_backend(1).is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let c = RequestContext {
        receivers,
        minor: 0,
        tables: Arc::default(),
        measurer: None,
    };
    Rig {
        c,
        inner,
        gate,
        close_gate,
        dir,
    }
}

#[tokio::test]
async fn close_while_opening_closes_after_open() {
    let (c, inner, gate, _dir) = gated().await;
    let opening = tokio::spawn({
        let c = c.clone();
        async move { dispatch(&c, req(1, open(9, 1, 7_100_000))).await }
    });
    let subscribing = tokio::spawn({
        let c = c.clone();
        async move { dispatch(&c, req(2, subscribe_cmd(4, 1, 7_100_000))).await }
    });
    while c.tables().channels.is_empty() || c.tables().subscriptions.is_empty() {
        tokio::task::yield_now().await;
    }
    // Updating a channel that is still opening: not found (yet).
    let upd = Command::UpdateChannel(v1::UpdateChannel {
        channel_id: 9,
        ..Default::default()
    });
    let (r, _) = dispatch(&c, req(3, upd)).await;
    assert_eq!(code(&r), v1::ResultCode::NotFound);
    let (r, _) = dispatch(
        &c,
        req(4, Command::CloseChannel(v1::CloseChannel { channel_id: 9 })),
    )
    .await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let un = Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall { subscription_id: 4 });
    let (r, _) = dispatch(&c, req(5, un)).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    gate.add_permits(2);
    let (r, media) = opening.await.expect("join");
    assert_eq!(code(&r), v1::ResultCode::Ok, "the open itself succeeded");
    assert!(media.is_none(), "but no audio is pumped");
    let (r, media) = subscribing.await.expect("join");
    assert_eq!(code(&r), v1::ResultCode::Ok);
    assert!(media.is_none());
    assert!(inner.channel_ids().is_empty(), "closed in the backend");
    assert!(inner.subscription_ids().is_empty());
    assert!(c.tables().channels.is_empty() && c.tables().subscriptions.is_empty());
}

fn unsub(id: u32) -> Command {
    Command::UnsubscribeWaterfall(v1::UnsubscribeWaterfall {
        subscription_id: id,
    })
}

/// STN-SUBORDER: a client that frees a place and takes it at once is never
/// refused, even when the requests finish in the opposite order.
#[tokio::test]
async fn unsubscribe_then_subscribe_at_the_limit_is_never_refused() {
    let (c, inner, gate, _dir) = gated().await;
    gate.add_permits(1000);
    let max = limits::waterfall_subscriptions_claim(1);
    for i in 1..=max {
        let (r, media) = dispatch(&c, req(u64::from(i), subscribe_cmd(i, 1, 7_100_000))).await;
        assert_eq!(code(&r), v1::ResultCode::Ok);
        assert!(media.is_some());
    }
    let (r, _) = dispatch(&c, req(100, subscribe_cmd(max + 1, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::ResourceExhausted, "full");
    // The read loop: both admitted, in the hub's order, before either runs.
    let first = admit(&c, req(101, unsub(1)));
    let second = admit(&c, req(102, subscribe_cmd(max + 1, 1, 7_100_000)));
    // The tasks run in the other order: the open waits for the close before
    // it, so the backend has room.
    let second = tokio::spawn({
        let c = c.clone();
        async move { finish(&c, second).await }
    });
    tokio::task::yield_now().await;
    let (r1, _) = finish(&c, first).await;
    assert_eq!(code(&r1), v1::ResultCode::Ok);
    let (r2, media) = second.await.expect("join");
    assert_eq!(code(&r2), v1::ResultCode::Ok, "not refused");
    assert!(media.is_some());
    let ids = inner.subscription_ids();
    assert_eq!(ids.len(), usize::try_from(max).expect("small"));
    assert!(!ids.contains(&1) && ids.contains(&(max + 1)));
    // The ID that was closed is gone.
    let (r, _) = dispatch(&c, req(103, unsub(1))).await;
    assert_eq!(code(&r), v1::ResultCode::NotFound);
}

/// STN-SUBORDER: an unsubscribe that arrives while its subscribe is still
/// opening cancels it, frees its place at once, and an open of the same ID
/// right after it waits for its close.
#[tokio::test]
async fn unsubscribe_during_opening_cancels() {
    let (c, inner, gate, _dir) = gated().await;
    let max = limits::waterfall_subscriptions_claim(1);
    let sub = admit(&c, req(1, subscribe_cmd(4, 1, 7_100_000)));
    let sub_task = tokio::spawn({
        let c = c.clone();
        async move { finish(&c, sub).await }
    });
    // The open is in flight (the gate is shut); the unsubscribe takes effect
    // in the read loop at once, answered without waiting for the open.
    let un = admit(&c, req(2, unsub(4)));
    assert_eq!(
        c.tables()
            .subscriptions
            .values()
            .filter(|e| e.is_live())
            .count(),
        0,
        "it no longer counts toward the limit"
    );
    let (r, _) = finish(&c, un).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    // The slot is free for `max` others while the old open still hangs.
    let mut held = Vec::new();
    for i in 1..=max {
        let a = admit(
            &c,
            req(10 + u64::from(i), subscribe_cmd(100 + i, 1, 7_100_000)),
        );
        assert!(
            !matches!(&a.0, Step::Answered(_)),
            "subscription {i} of {max} admitted"
        );
        held.push(a);
    }
    drop(held);
    for i in 1..=max {
        c.tables().subscriptions.remove(&(100 + i));
    }
    // The same ID again, at once: reserved now, backend call after the old
    // one's close.
    let again = admit(&c, req(3, subscribe_cmd(4, 1, 7_100_000)));
    assert!(!matches!(&again.0, Step::Answered(_)));
    let again_task = tokio::spawn({
        let c = c.clone();
        async move { finish(&c, again).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inner.subscription_ids().is_empty(), "nothing opened yet");
    gate.add_permits(2);
    let (r, media) = sub_task.await.expect("join");
    assert_eq!(code(&r), v1::ResultCode::Ok, "the open itself succeeded");
    assert!(media.is_none(), "but it never went live");
    let (r, media) = again_task.await.expect("join");
    assert_eq!(code(&r), v1::ResultCode::Ok);
    assert!(media.is_some(), "the later open is live");
    assert_eq!(inner.subscription_ids(), vec![4], "and is the one left");
    assert_eq!(c.tables().open_subscriptions(), 1);
}

fn close_cmd(id: u32) -> Command {
    Command::CloseChannel(v1::CloseChannel { channel_id: id })
}

fn is_answered(a: &Admitted) -> Option<v1::ResultCode> {
    match &a.0 {
        Step::Answered(r) => Some(code(r)),
        _ => None,
    }
}

fn spawn_finish(
    c: &RequestContext,
    a: Admitted,
) -> tokio::task::JoinHandle<(v1::Response, Option<Media>)> {
    let c = c.clone();
    tokio::spawn(async move { finish(&c, a).await })
}

/// An open channel `id` on the rig's receiver, through the whole of
/// `dispatch`.
async fn opened(c: &RequestContext, id: u32) -> Media {
    let (r, media) = dispatch(c, req(u64::from(id), open(id, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok, "open {id}");
    media.expect("media")
}

/// A status tick holds the receivers' lock and an `UpdateChannel` needs both:
/// the update must not hold the tables' lock while it waits for the
/// receivers' (STN-SUBORDER-F D).
/// (The runtime has threads to spare: the receivers' own tasks block on the
/// same lock while the status holds it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn update_channel_never_holds_the_tables_while_taking_the_receivers() {
    let r = rig().await;
    r.gate.add_permits(10);
    let c = r.c.clone();
    let _media = opened(&c, 5).await;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let receivers = c.receivers.clone();
    // The status: inside `health`, with the receivers' lock held.
    let status = tokio::task::spawn_blocking(move || {
        receivers.health(|_| {
            let _ = entered_tx.send(());
            let _ = release_rx.lock().expect("lock").recv();
            0
        })
    });
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .expect("join")
        .expect("inside health");
    let upd = Command::UpdateChannel(v1::UpdateChannel {
        channel_id: 5,
        frequency_hz: Some(7_050_000),
        ..Default::default()
    });
    let update = tokio::spawn({
        let c = c.clone();
        async move { dispatch(&c, req(6, upd)).await }
    });
    // However far the update has got, the tables are free for the status's
    // own look at them.
    // (A blocking sleep: the runtime's own timer needs worker threads, which
    // the receivers' tasks may be holding while they wait for the lock.)
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(200)))
        .await
        .expect("join");
    let free = c.tables.try_lock().is_ok();
    // Released before any assertion, so a failure is a failure and not a hang.
    release_tx.send(()).expect("release");
    let (resp, _) = update.await.expect("join");
    status.await.expect("join");
    assert!(
        free,
        "the update holds the tables while it waits for the receivers"
    );
    assert_eq!(code(&resp), v1::ResultCode::Ok);
}

#[tokio::test]
async fn the_status_counts_per_receiver_from_one_look() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let _a = dispatch(&c, req(1, open(1, 1, 7_100_000))).await;
    let _b = dispatch(&c, req(2, open(2, 1, 7_100_000))).await;
    let _d = dispatch(&c, req(3, open(3, 2, 14_200_000))).await;
    let counts = c.tables().open_channels_by_receiver();
    assert_eq!(counts.get(&1), Some(&2));
    assert_eq!(counts.get(&2), Some(&1));
    assert_eq!(counts.len(), 2);
}

/// The channel version of the at-the-limit case.
#[tokio::test]
async fn close_then_open_at_the_channel_limit_is_never_refused() {
    let r = rig().await;
    r.gate.add_permits(1000);
    let c = &r.c;
    let max = limits::CHANNELS_PER_RECEIVER;
    for i in 1..=max {
        let _ = opened(c, i).await;
    }
    let (full, _) = dispatch(c, req(100, open(max + 1, 1, 7_100_000))).await;
    assert_eq!(code(&full), v1::ResultCode::ResourceExhausted);
    let first = admit(c, req(101, close_cmd(1)));
    let second = admit(c, req(102, open(max + 1, 1, 7_100_000)));
    assert!(is_answered(&second).is_none(), "admitted, not refused");
    let second = spawn_finish(c, second);
    tokio::task::yield_now().await;
    let (r1, _) = finish(c, first).await;
    assert_eq!(code(&r1), v1::ResultCode::Ok);
    let (r2, media) = second.await.expect("join");
    assert_eq!(code(&r2), v1::ResultCode::Ok);
    assert!(media.is_some());
    let ids = r.inner.channel_ids();
    assert_eq!(ids.len(), usize::try_from(max).expect("small"));
    assert!(!ids.contains(&1) && ids.contains(&(max + 1)));
}

#[tokio::test]
async fn a_second_close_while_the_first_is_closing_is_not_found() {
    let r = rig().await;
    r.gate.add_permits(10);
    let c = &r.c;
    let _media = opened(c, 1).await;
    r.close_gate.forget_permits(1 << 20);
    let first = spawn_finish(c, admit(c, req(10, close_cmd(1))));
    tokio::task::yield_now().await;
    let second = admit(c, req(11, close_cmd(1)));
    assert_eq!(is_answered(&second), Some(v1::ResultCode::NotFound));
    let (resp, _) = finish(c, second).await;
    assert_eq!(code(&resp), v1::ResultCode::NotFound);
    r.close_gate.add_permits(1);
    let (resp, _) = first.await.expect("join");
    assert_eq!(code(&resp), v1::ResultCode::Ok);
    assert!(c.tables().channels.is_empty());
}

/// S1: an ID closing on one receiver is not taken by an open on another; on
/// the same receiver it is.
#[tokio::test]
async fn an_id_still_closing_on_another_receiver_is_already_exists() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let (r, _m) = dispatch(&c, req(1, open(7, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let closing = admit(&c, req(2, close_cmd(7)));
    let elsewhere = admit(&c, req(3, open(7, 2, 14_200_000)));
    assert_eq!(is_answered(&elsewhere), Some(v1::ResultCode::AlreadyExists));
    let same = admit(&c, req(4, open(7, 1, 7_100_000)));
    assert!(is_answered(&same).is_none(), "the same receiver is safe");
    let (r, _) = finish(&c, closing).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let (r, media) = finish(&c, same).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    assert!(media.is_some());
    // Settled: another receiver may take the ID once this one is closed.
    let (r, _) = dispatch(&c, req(5, close_cmd(7))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let (r, _) = dispatch(&c, req(6, open(7, 2, 14_200_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    // The same for a subscription.
    let (r, _m) = dispatch(&c, req(7, subscribe_cmd(8, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let closing = admit(&c, req(8, unsub(8)));
    let elsewhere = admit(&c, req(9, subscribe_cmd(8, 2, 14_200_000)));
    assert_eq!(is_answered(&elsewhere), Some(v1::ResultCode::AlreadyExists));
    let _ = finish(&c, closing).await;
}

/// S1: the backend's own closes and a receiver going never remove an
/// opening or a closing entry.
#[tokio::test]
async fn backend_closes_leave_opening_and_closing_entries() {
    let r = rig().await;
    let c = &r.c;
    r.gate.add_permits(1000);
    let _open = opened(c, 1).await;
    let _gone = opened(c, 2).await;
    r.close_gate.forget_permits(1 << 20);
    // 1 closing in its backend; 3 opening (the gate is shut).
    let closing = spawn_finish(c, admit(c, req(10, close_cmd(1))));
    tokio::task::yield_now().await;
    r.gate.forget_permits(1000);
    let opening = spawn_finish(c, admit(c, req(11, open(3, 1, 7_100_000))));
    tokio::task::yield_now().await;
    c.tables().channel_closed(1);
    assert!(
        matches!(c.tables().channels[&1].slot, Slot::Closing { .. }),
        "a closing entry stays"
    );
    c.tables().channel_closed(3);
    assert!(matches!(
        c.tables().channels[&3].slot,
        Slot::Opening {
            close_requested: true,
            ..
        }
    ));
    // The receiver went: the open entry (2) goes, the others stay.
    c.tables().receiver_gone(1);
    assert!(!c.tables().channels.contains_key(&2));
    assert!(matches!(c.tables().channels[&1].slot, Slot::Closing { .. }));
    assert!(matches!(
        c.tables().channels[&3].slot,
        Slot::Opening {
            close_requested: true,
            ..
        }
    ));
    // Both end through their own backend calls.
    r.close_gate.add_permits(1 << 20);
    r.gate.add_permits(10);
    let (resp, _) = closing.await.expect("join");
    assert_eq!(code(&resp), v1::ResultCode::Ok);
    let (resp, media) = opening.await.expect("join");
    assert_eq!(code(&resp), v1::ResultCode::Ok);
    assert!(media.is_none(), "closed while it was opening");
    assert!(c.tables().channels.is_empty());
    assert!(!r.inner.channel_ids().contains(&3));
}

/// S2: the wait is bounded: another ID goes ahead, the same ID is answered
/// `BACKEND_ERROR`.
#[tokio::test(start_paused = true)]
async fn the_wait_for_a_close_is_bounded() {
    let r = rig().await;
    r.gate.add_permits(1000);
    let c = &r.c;
    let _a = opened(c, 1).await;
    r.close_gate.forget_permits(1 << 20);
    let closing = spawn_finish(c, admit(c, req(10, close_cmd(1))));
    tokio::task::yield_now().await;
    // A different ID waits, then goes ahead and the backend has room.
    let (resp, media) = dispatch(c, req(11, open(2, 1, 7_100_000))).await;
    assert_eq!(code(&resp), v1::ResultCode::Ok);
    assert!(media.is_some());
    // The same ID is answered BACKEND_ERROR after the wait.
    let again = admit(c, req(12, open(1, 1, 7_100_000)));
    assert!(is_answered(&again).is_none());
    let (resp, media) = finish(c, again).await;
    assert_eq!(code(&resp), v1::ResultCode::BackendError);
    assert!(media.is_none());
    assert!(
        !c.tables().channels.contains_key(&1),
        "the entry is released"
    );
    r.close_gate.add_permits(1 << 20);
    let (resp, _) = closing.await.expect("join");
    assert_eq!(code(&resp), v1::ResultCode::Ok);
}

/// S3: open and close pairs cannot pile up: the entries winding down count
/// up to as many again as the limit.
#[tokio::test]
async fn closing_entries_are_bounded() {
    let r = rig().await;
    let c = &r.c;
    let max = limits::CHANNELS_PER_RECEIVER;
    let mut held = Vec::new();
    let mut id = 0;
    for _ in 0..2 {
        let mut round = Vec::new();
        for _ in 0..max {
            id += 1;
            let a = admit(c, req(u64::from(id), open(id, 1, 7_100_000)));
            assert!(is_answered(&a).is_none(), "open {id} admitted");
            round.push(id);
            held.push(a);
        }
        for i in round {
            let a = admit(c, req(1000 + u64::from(i), close_cmd(i)));
            assert_eq!(is_answered(&a), Some(v1::ResultCode::Ok), "close {i}");
            held.push(a);
        }
    }
    assert_eq!(
        c.tables().channels.values().filter(|e| e.is_live()).count(),
        0
    );
    let a = admit(c, req(2000, open(id + 1, 1, 7_100_000)));
    assert_eq!(is_answered(&a), Some(v1::ResultCode::ResourceExhausted));
    // The same for subscriptions, station-wide.
    let subs = limits::waterfall_subscriptions_claim(1);
    let mut sid = 0;
    for _ in 0..2 {
        let mut round = Vec::new();
        for _ in 0..subs {
            sid += 1;
            let a = admit(
                c,
                req(3000 + u64::from(sid), subscribe_cmd(sid, 1, 7_100_000)),
            );
            assert!(is_answered(&a).is_none(), "subscription {sid} admitted");
            round.push(sid);
            held.push(a);
        }
        for i in round {
            held.push(admit(c, req(4000 + u64::from(i), unsub(i))));
        }
    }
    let a = admit(c, req(5000, subscribe_cmd(sid + 1, 1, 7_100_000)));
    assert_eq!(is_answered(&a), Some(v1::ResultCode::ResourceExhausted));
    drop(held);
}

/// S4: a close takes its backend when it is admitted, so it reaches the
/// backend its channel was opened on even if the receiver goes before the
/// close runs.
#[tokio::test]
async fn a_close_keeps_the_backend_it_was_admitted_with() {
    let st = Station::standard().await;
    let c = ctx_for(&st);
    let (r, _m) = dispatch(&c, req(1, open(4, 1, 7_100_000))).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    let backend = st.backends.get(1).expect("backend");
    assert_eq!(backend.channel_ids(), vec![4]);
    let closing = admit(&c, req(2, close_cmd(4)));
    st.receivers
        .configure(&v1::ConfigureReceivers {
            receivers: vec![receiver(2, "CH000002", ProfileId::RtlsdrV4, 14_200_000)],
        })
        .await
        .expect("set");
    let (r, _) = finish(&c, closing).await;
    assert_eq!(code(&r), v1::ResultCode::Ok);
    assert!(backend.channel_ids().is_empty(), "closed in its backend");
}

/// N1: an open closed while it waited never calls its backend.
#[tokio::test]
async fn an_open_closed_while_it_waited_skips_the_backend() {
    let r = rig().await;
    r.gate.add_permits(1000);
    let c = &r.c;
    let _a = opened(c, 1).await;
    r.close_gate.forget_permits(1 << 20);
    let closing = spawn_finish(c, admit(c, req(10, close_cmd(1))));
    tokio::task::yield_now().await;
    let waiting = spawn_finish(c, admit(c, req(11, open(2, 1, 7_100_000))));
    tokio::task::yield_now().await;
    let (resp, _) = dispatch(c, req(12, close_cmd(2))).await;
    assert_eq!(code(&resp), v1::ResultCode::Ok);
    let permits = r.gate.available_permits();
    r.close_gate.add_permits(1 << 20);
    let (resp, media) = waiting.await.expect("join");
    assert_eq!(code(&resp), v1::ResultCode::Ok);
    assert!(
        matches!(resp.payload, Some(Payload::ChannelOpened(ref o)) if o.channel_id == 2 && o.receiver_id == 1)
    );
    assert!(media.is_none());
    assert_eq!(r.gate.available_permits(), permits, "no backend call");
    assert!(!r.inner.channel_ids().contains(&2));
    let _ = closing.await.expect("join");
    assert!(c.tables().channels.is_empty());
}

/// The session ends with an open waiting and an entry closing: the tasks are
/// aborted and both are closed in their backends.
#[tokio::test]
async fn the_session_ending_closes_a_waiting_open_and_a_closing_entry() {
    let r = rig().await;
    r.gate.add_permits(1000);
    let c = &r.c;
    let _a = opened(c, 1).await;
    r.close_gate.forget_permits(1 << 20);
    let closing = spawn_finish(c, admit(c, req(10, close_cmd(1))));
    tokio::task::yield_now().await;
    let waiting = spawn_finish(c, admit(c, req(11, open(2, 1, 7_100_000))));
    tokio::task::yield_now().await;
    closing.abort();
    waiting.abort();
    let _ = closing.await;
    let _ = waiting.await;
    r.close_gate.add_permits(1 << 20);
    let (channels, subs) = c.tables().take_all();
    assert_eq!(channels, vec![(1, 1), (1, 2)]);
    assert!(subs.is_empty());
    close_in_backends(&c.receivers, channels, subs).await;
    assert!(r.inner.channel_ids().is_empty());
    assert!(c.tables().channels.is_empty());
}

#[test]
fn backend_errors_never_say_ok() {
    let e = BackendError::new(v1::ResultCode::Ok, "weird");
    assert_eq!(code(&backend_error(1, &e)), v1::ResultCode::BackendError);
    let long = BackendError::backend("x".repeat(1000));
    let r = backend_error(1, &long);
    assert_eq!(
        r.result.map(|r| r.message.len()),
        Some(limits::MAX_DETAIL_BYTES)
    );
    assert_eq!(
        format!(
            "{:?}",
            Media::Rows {
                subscription_id: 3,
                stream: Box::pin(futures_util::stream::empty()),
                stop: CancellationToken::new()
            }
        ),
        "Rows { subscription_id: 3, .. }"
    );
}
