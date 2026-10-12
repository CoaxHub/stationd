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

/// A backend whose `open_channel`/`subscribe_waterfall` wait for a gate.
struct Gated {
    inner: FakeBackend,
    gate: Arc<tokio::sync::Semaphore>,
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
        self.inner.unsubscribe_waterfall(id).await
    }
    fn health(&self) -> station_backend::Health {
        self.inner.health()
    }
    fn events(&self) -> station_backend::EventStream {
        self.inner.events()
    }
}

#[tokio::test]
async fn close_while_opening_closes_after_open() {
    let inner = FakeBackend::default();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
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
