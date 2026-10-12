// SPDX-License-Identifier: GPL-3.0-only
//! Hermetic tests of `Ka9qBackend` (the `station_backend::Backend` for
//! radiod) against the fake radiod in `support/`, on Tokio's paused clock.

#![allow(clippy::float_cmp, reason = "values are copied, not computed")]
#![allow(clippy::too_many_lines, reason = "scenario tests read top to bottom")]

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use backend_ka9q::backend::{Ka9qBackend, Ka9qConfig};
use backend_ka9q::client::ClientConfig;
use futures_util::StreamExt as _;
use station_backend::{AudioStream, Backend, BackendError, ChannelOutput};
use station_proto::v1;
use station_proto::validate::{WaterfallDecoder, sanitize_capabilities};
use support::FakeRadiod;

fn local0() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn config(fake: &FakeRadiod, data: Option<SocketAddr>) -> Ka9qConfig {
    let mut c = Ka9qConfig::new(fake.addr, data);
    c.client = ClientConfig {
        command_timeout: Duration::from_millis(100),
        lifetime_frames: 250, // 5 s
        ..c.client
    };
    c
}

async fn setup() -> (FakeRadiod, Ka9qBackend) {
    let fake = FakeRadiod::start().await;
    let backend = Ka9qBackend::new(config(&fake, Some(local0()))).unwrap();
    let client = backend.client().await.unwrap();
    fake.set_rtp_dest(client.local_data_addr());
    fake.set_status_dest(client.local_channel_status_addr().unwrap());
    (fake, backend)
}

fn opus(sample_rate_hz: u32) -> v1::AudioParams {
    v1::AudioParams {
        codec: v1::AudioCodec::Opus as i32,
        bitrate_bps: 16_000,
        sample_rate_hz,
        frame_duration_ms: 20,
    }
}

fn open_req(channel_id: u32, frequency_hz: u64, mode: v1::Mode) -> v1::OpenChannel {
    v1::OpenChannel {
        channel_id,
        receiver_id: 1,
        frequency_hz,
        mode: mode as i32,
        low_cut_hz: -3000,
        high_cut_hz: 3000,
        audio: Some(opus(12_000)),
        squelch_snr_db: None,
    }
}

fn code<T>(r: Result<T, BackendError>) -> v1::ResultCode {
    match r {
        Ok(_) => v1::ResultCode::Ok,
        Err(e) => e.code,
    }
}

/// The next audio frame, skipping signals.
async fn next_audio(s: &mut AudioStream) -> v1::AudioFrame {
    loop {
        match s.next().await.expect("stream ended") {
            ChannelOutput::Audio(a) => return a,
            ChannelOutput::Signal(_) => {}
        }
    }
}

#[tokio::test(start_paused = true)]
async fn capabilities_and_health_from_radiod_status() {
    let fake = FakeRadiod::start().await;
    let backend = Ka9qBackend::new(config(&fake, Some(local0()))).unwrap();
    let info = backend.info();
    assert_eq!(info.kind, v1::BackendKind::Ka9qRadio as i32);
    assert!(info.version.starts_with("ka9q-radio "), "{}", info.version);
    // Nothing heard yet: down.
    let h = backend.health();
    assert_eq!(h.backend.state, v1::backend_health::State::Down as i32);
    assert!(h.frontend.is_none());

    let caps = backend.capabilities().await.unwrap();
    let station = v1::Capabilities {
        receivers: vec![v1::ReceiverCapabilities {
            receiver_id: 1,
            ..caps.receiver.clone()
        }],
        modes: caps.modes.clone(),
        codecs: caps.codecs.clone(),
        waterfall: caps.waterfall,
        measurements: None,
    };
    let sanitized = sanitize_capabilities(&station, 0).unwrap();
    assert!(sanitized.clamped.is_empty(), "{:?}", sanitized.clamped);
    assert_eq!(
        caps.receiver.tunable_range,
        Some(v1::FrequencyRange {
            start_hz: 0,
            end_hz: 12_000_000
        })
    );
    assert_eq!(caps.receiver.sample_rate_hz, 24_000_000);
    assert_eq!(caps.receiver.sdr_model, "fake radiod");
    assert_eq!(caps.modes.len(), 6);
    assert_eq!(caps.codecs, vec![v1::AudioCodec::Opus as i32]);

    let h = backend.health();
    assert_eq!(h.backend.state, v1::backend_health::State::Running as i32);
    let fe = h.frontend.expect("front end");
    assert_eq!(fe.state, v1::frontend_health::State::Ok as i32);
    assert_eq!(fe.sample_rate_hz, 24_000_000);
    assert_eq!(fe.center_frequency_hz, 6_000_000);
    assert!((fe.input_power_dbfs.unwrap() - (-20.4)).abs() < 0.01);
    // The probe channel was destroyed again.
    assert!(
        fake.wait_until(Duration::from_millis(200), |s| s.channels.is_empty())
            .await
    );
    fake.assert_client_contract();

    // radiod stops answering: the next command times out, health is DOWN.
    fake.state().mute = true;
    tokio::time::sleep(Duration::from_secs(6)).await; // status no longer fresh
    assert_eq!(
        code(backend.capabilities().await),
        v1::ResultCode::BackendError
    );
    assert_eq!(
        backend.health().backend.state,
        v1::backend_health::State::Down as i32
    );
}

#[tokio::test(start_paused = true)]
async fn audio_channel_lifecycle() {
    let (fake, backend) = setup().await;
    let (opened, mut audio) = backend
        .open_channel(open_req(1, 10_000_000, v1::Mode::Am))
        .await
        .unwrap();
    assert_eq!(
        opened,
        v1::ChannelOpened {
            channel_id: 1,
            receiver_id: 1,
            frequency_hz: 10_000_000,
            mode: v1::Mode::Am as i32,
            low_cut_hz: -3000,
            high_cut_hz: 3000,
            audio: Some(opus(12_000)),
        }
    );
    assert_eq!(backend.channel_ids(), vec![1]);
    let ssrc = backend.channel_ssrc(1).await.unwrap();
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!(fc.preset, "am");
    assert_eq!(
        (fc.encoding, fc.samprate, fc.opus_bitrate),
        (3, 12_000, 16_000)
    );
    assert_eq!(fc.output_channels, 1);
    assert!(!fc.snr_squelch);
    assert_eq!(fc.squelch_open, -100.0, "squelch off keeps FM open too");

    let mut frames = Vec::new();
    let mut signals = Vec::new();
    while frames.len() < 60 {
        match audio.next().await.unwrap() {
            ChannelOutput::Audio(a) => frames.push(a),
            ChannelOutput::Signal(s) => signals.push(s),
        }
    }
    for (i, f) in frames.iter().enumerate() {
        let i = u32::try_from(i).unwrap();
        assert_eq!((f.channel_id, f.seq, f.timestamp), (1, i, i * 960));
        assert_eq!(f.payload[0], 0x28);
    }
    // Unsolicited status every 0.5 s: about two signals in 1.2 s.
    assert!(!signals.is_empty() && signals.len() <= 6, "{signals:?}");
    for s in &signals {
        assert_eq!(s.channel_id, 1);
        assert!(s.snr_db > 20.0 && s.snr_db < 50.0, "{s:?}");
        assert!((s.power_dbfs - (-19.5)).abs() < 0.01);
        assert_eq!(s.squelch_open, None);
    }

    // Retune in place: the sequence continues.
    let updated = backend
        .update_channel(v1::UpdateChannel {
            channel_id: 1,
            frequency_hz: Some(9_000_000),
            low_cut_hz: Some(-4000),
            high_cut_hz: Some(4000),
            squelch_snr_db: Some(10.0),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        updated,
        v1::ChannelUpdated {
            channel_id: 1,
            frequency_hz: 9_000_000,
            mode: v1::Mode::Am as i32,
            low_cut_hz: -4000,
            high_cut_hz: 4000,
        }
    );
    let last = frames.last().unwrap().seq;
    let next = next_audio(&mut audio).await;
    assert!(
        next.seq > last && next.seq <= last + 3,
        "{} -> {}",
        last,
        next.seq
    );
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!(fc.frequency, 9e6);
    assert!(fc.snr_squelch);
    assert_eq!((fc.squelch_open, fc.squelch_close), (10.0, 9.0));
    // Signals now say whether the squelch is open.
    let signal = loop {
        if let ChannelOutput::Signal(s) = audio.next().await.unwrap() {
            break s;
        }
    };
    assert_eq!(signal.squelch_open, Some(true));

    // Mode change: the full configuration is re-sent with the new preset.
    let updated = backend
        .update_channel(v1::UpdateChannel {
            channel_id: 1,
            mode: Some(v1::Mode::Usb as i32),
            squelch_off: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(updated.mode, v1::Mode::Usb as i32);
    assert_eq!(updated.frequency_hz, 9_000_000);
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!(fc.preset, "usb");
    assert_eq!((fc.low_edge, fc.high_edge), (-4000.0, 4000.0), "edges kept");
    assert!(!fc.snr_squelch);
    assert_eq!(fc.encoding, 3);

    // Errors.
    let upd = |u: v1::UpdateChannel| backend.update_channel(u);
    assert_eq!(
        code(
            upd(v1::UpdateChannel {
                channel_id: 9,
                ..Default::default()
            })
            .await
        ),
        v1::ResultCode::NotFound
    );
    assert_eq!(
        code(
            upd(v1::UpdateChannel {
                channel_id: 1,
                frequency_hz: Some(50_000_000),
                ..Default::default()
            })
            .await
        ),
        v1::ResultCode::OutOfRange
    );
    assert_eq!(
        code(
            upd(v1::UpdateChannel {
                channel_id: 1,
                mode: Some(0),
                ..Default::default()
            })
            .await
        ),
        v1::ResultCode::InvalidArgument
    );
    assert_eq!(
        code(
            upd(v1::UpdateChannel {
                channel_id: 1,
                low_cut_hz: Some(5000),
                ..Default::default()
            })
            .await
        ),
        v1::ResultCode::InvalidArgument
    );
    assert_eq!(
        code(
            upd(v1::UpdateChannel {
                channel_id: 1,
                squelch_snr_db: Some(f32::NAN),
                ..Default::default()
            })
            .await
        ),
        v1::ResultCode::InvalidArgument
    );

    // Close: LIFETIME 1, the stream ends, the ID is free again.
    backend.close_channel(1).await.unwrap();
    while audio.next().await.is_some() {}
    assert!(
        fake.wait_until(Duration::from_millis(200), |s| !s
            .channels
            .contains_key(&ssrc))
            .await
    );
    assert_eq!(
        fake.state()
            .log
            .iter()
            .rfind(|c| c.ssrc == ssrc)
            .unwrap()
            .lifetime,
        Some(1)
    );
    assert_eq!(
        code(backend.close_channel(1).await),
        v1::ResultCode::NotFound
    );
    assert!(backend.channel_ids().is_empty());
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn open_channel_checks_and_reports_what_radiod_runs() {
    let (fake, backend) = setup().await;
    let _a = backend
        .open_channel(open_req(1, 7_100_000, v1::Mode::Lsb))
        .await
        .unwrap();
    assert_eq!(
        code(
            backend
                .open_channel(open_req(1, 7_100_000, v1::Mode::Lsb))
                .await
        ),
        v1::ResultCode::AlreadyExists
    );
    let open = |r| backend.open_channel(r);
    assert_eq!(
        code(open(open_req(2, 30_000_000, v1::Mode::Am)).await),
        v1::ResultCode::OutOfRange
    );
    assert_eq!(
        code(open(open_req(2, 0, v1::Mode::Am)).await),
        v1::ResultCode::OutOfRange
    );
    assert_eq!(
        code(open(open_req(2, 7_000_000, v1::Mode::Unspecified)).await),
        v1::ResultCode::InvalidArgument
    );
    let mut r = open_req(2, 7_000_000, v1::Mode::Am);
    r.mode = 42;
    assert_eq!(code(open(r).await), v1::ResultCode::Unsupported);
    let mut r = open_req(2, 7_000_000, v1::Mode::Am);
    r.audio = None;
    assert_eq!(code(open(r).await), v1::ResultCode::InvalidArgument);
    let mut r = open_req(2, 7_000_000, v1::Mode::Am);
    r.audio = Some(v1::AudioParams {
        codec: 5,
        ..opus(12_000)
    });
    assert_eq!(code(open(r).await), v1::ResultCode::Unsupported);
    let mut r = open_req(2, 7_000_000, v1::Mode::Am);
    r.low_cut_hz = 3000;
    assert_eq!(code(open(r).await), v1::ResultCode::InvalidArgument);
    let mut r = open_req(2, 7_000_000, v1::Mode::Am);
    r.squelch_snr_db = Some(f32::INFINITY);
    assert_eq!(code(open(r).await), v1::ResultCode::InvalidArgument);
    assert_eq!(backend.channel_ids(), vec![1]);

    // Broadcast FM: radiod forces 48 kHz, and ChannelOpened says so.
    let mut r = open_req(3, 10_700_000, v1::Mode::Fm);
    r.audio = Some(opus(24_000));
    r.low_cut_hz = 0;
    r.high_cut_hz = 0;
    r.squelch_snr_db = Some(6.0);
    let (opened, _s) = backend.open_channel(r).await.unwrap();
    assert_eq!(opened.audio.unwrap().sample_rate_hz, 48_000);
    assert_eq!(
        (opened.low_cut_hz, opened.high_cut_hz),
        (-110_000, 110_000),
        "the preset's edges"
    );
    let fc = fake
        .channel(backend.channel_ssrc(3).await.unwrap())
        .unwrap();
    assert_eq!((fc.preset.as_str(), fc.demod), ("wfm", 2));
    assert_eq!((fc.squelch_open, fc.squelch_close), (6.0, 5.0));

    // A channel whose frequency radiod does not apply fails and is removed.
    fake.state().ignore_frequency = true;
    assert_eq!(
        code(open(open_req(4, 7_000_000, v1::Mode::Am)).await),
        v1::ResultCode::BackendError
    );
    fake.state().ignore_frequency = false;
    assert_eq!(backend.channel_ids(), vec![1, 3]);
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn waterfall_rows_are_decodable_and_paced() {
    let (fake, backend) = setup().await;
    let range = v1::FrequencyRange {
        start_hz: 0,
        end_hz: 12_000_000,
    };
    let req = v1::SubscribeWaterfall {
        subscription_id: 5,
        receiver_id: 1,
        range: Some(range),
        bins: 256,
        row_interval_ms: 100,
        quantization: None,
        compression: v1::WaterfallCompression::Zstd as i32,
    };
    let (sub, mut rows) = backend.subscribe_waterfall(req).await.unwrap();
    assert_eq!(sub.subscription_id, 5);
    assert_eq!((sub.bins, sub.row_interval_ms), (256, 100));
    // The fake is a real 24 MS/s front end at LO 0 (L 480,000, M 120,001):
    // 256 bins of 24 MS/s / 514 = 46,692.6 Hz at analysis shift 129, the
    // widest bins radiod can put inside 0 .. 12 MHz without its window
    // reaching DC or Nyquist (radiod-notes.md §10).
    assert_eq!(
        sub.range,
        Some(v1::FrequencyRange {
            start_hz: 23_346,
            end_hz: 11_976_654,
        })
    );
    assert_eq!(
        sub.quantization,
        Some(v1::Quantization {
            offset_db: -130.0,
            step_db: 0.5
        })
    );
    let ssrc = *fake.state().created.last().unwrap();
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!((fc.demod, fc.bin_count), (4, 256));
    assert!((fc.rbw - 24e6 / 514.0).abs() < 0.01, "{}", fc.rbw);
    // Front-end shift 150,584 (40 Hz bins): 150,584 × 514 / 600,000 → 129.
    assert_eq!(fc.frequency, 150_584.0 * 40.0);

    let start = tokio::time::Instant::now();
    let mut decoder = WaterfallDecoder::new();
    let mut out = Vec::new();
    for seq in 0..20 {
        let row = rows.next().await.unwrap();
        assert_eq!((row.subscription_id, row.seq), (5, seq));
        decoder
            .decode(&row.payload, sub.compression, 256, &mut out)
            .unwrap();
        // The fake's carrier sits in the center bin.
        let peak = out.iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0;
        assert_eq!(peak, 128);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1800) && elapsed <= Duration::from_millis(2300),
        "20 rows took {elapsed:?}"
    );
    assert_eq!(
        code(backend.subscribe_waterfall(req).await),
        v1::ResultCode::AlreadyExists
    );
    backend.unsubscribe_waterfall(5).await.unwrap();
    assert!(rows.next().await.is_none());
    assert!(
        fake.wait_until(Duration::from_millis(200), |s| !s
            .channels
            .contains_key(&ssrc))
            .await
    );
    assert_eq!(
        code(backend.unsubscribe_waterfall(5).await),
        v1::ResultCode::NotFound
    );
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn waterfall_request_checks() {
    let (_fake, backend) = setup().await;
    let base = v1::SubscribeWaterfall {
        subscription_id: 1,
        receiver_id: 1,
        range: Some(v1::FrequencyRange {
            start_hz: 9_000_000,
            end_hz: 11_000_000,
        }),
        bins: 512,
        row_interval_ms: 1,
        quantization: Some(v1::Quantization {
            offset_db: -120.0,
            step_db: 0.25,
        }),
        compression: v1::WaterfallCompression::None as i32,
    };
    let (sub, mut rows) = backend.subscribe_waterfall(base).await.unwrap();
    assert_eq!(sub.row_interval_ms, 50, "clamped to the fastest rate");
    assert_eq!(sub.quantization, base.quantization);
    assert_eq!(rows.next().await.unwrap().payload.len(), 512);
    let sub_code = |f: fn(&mut v1::SubscribeWaterfall)| {
        let mut r = base;
        r.subscription_id = 2;
        f(&mut r);
        let b = backend.clone();
        async move { code(b.subscribe_waterfall(r).await) }
    };
    assert_eq!(sub_code(|r| r.bins = 10).await, v1::ResultCode::OutOfRange);
    assert_eq!(
        sub_code(|r| r.bins = 100_000).await,
        v1::ResultCode::OutOfRange
    );
    assert_eq!(
        sub_code(|r| r.compression = 0).await,
        v1::ResultCode::InvalidArgument
    );
    assert_eq!(
        sub_code(|r| r.compression = 9).await,
        v1::ResultCode::Unsupported
    );
    assert_eq!(
        sub_code(|r| r.range = None).await,
        v1::ResultCode::InvalidArgument
    );
    assert_eq!(
        sub_code(|r| r.range = Some(v1::FrequencyRange {
            start_hz: 5,
            end_hz: 5
        }))
        .await,
        v1::ResultCode::InvalidArgument
    );
    assert_eq!(
        sub_code(|r| r.range = Some(v1::FrequencyRange {
            start_hz: 11_000_000,
            end_hz: 13_000_000
        }))
        .await,
        v1::ResultCode::OutOfRange
    );
    assert_eq!(
        sub_code(|r| r.row_interval_ms = 60_000).await,
        v1::ResultCode::OutOfRange
    );
    assert_eq!(
        sub_code(|r| r.quantization = Some(v1::Quantization {
            offset_db: 0.0,
            step_db: -1.0
        }))
        .await,
        v1::ResultCode::InvalidArgument
    );
    assert_eq!(backend.subscription_ids(), vec![1]);
}

#[tokio::test(start_paused = true)]
async fn set_frontend_retunes_only_when_radiod_does() {
    let (fake, backend) = setup().await;
    let mut events = backend.events();
    for req in [
        v1::SetFrontend {
            gain_db: Some(10.0),
            ..Default::default()
        },
        v1::SetFrontend {
            hardware_agc: Some(true),
            ..Default::default()
        },
        v1::SetFrontend {
            antenna_port: Some("A".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            code(backend.set_frontend(req).await),
            v1::ResultCode::Unsupported
        );
    }
    // No change requested: the current state.
    let state = backend
        .set_frontend(v1::SetFrontend::default())
        .await
        .unwrap();
    assert_eq!(state.center_frequency_hz, 6_000_000);
    // The signal generator cannot retune.
    let retune = v1::SetFrontend {
        center_frequency_hz: Some(20_000_000),
        ..Default::default()
    };
    assert_eq!(
        code(backend.set_frontend(retune.clone()).await),
        v1::ResultCode::Unsupported
    );
    assert_eq!(
        code(
            backend
                .set_frontend(v1::SetFrontend {
                    center_frequency_hz: Some(7_000_000_000),
                    ..Default::default()
                })
                .await
        ),
        v1::ResultCode::OutOfRange
    );

    // A tunable front end: channels outside the new passband are closed.
    let (_, mut audio) = backend
        .open_channel(open_req(1, 10_000_000, v1::Mode::Am))
        .await
        .unwrap();
    fake.state().fe_tunable = true;
    let state = backend.set_frontend(retune).await.unwrap();
    assert_eq!(
        state.tunable_range,
        Some(v1::FrequencyRange {
            start_hz: 20_000_000,
            end_hz: 32_000_000
        })
    );
    assert!(backend.channel_ids().is_empty());
    while audio.next().await.is_some() {}
    let closed = events.next().await.unwrap();
    assert_eq!(closed.kind, v1::event::Kind::ChannelClosed as i32);
    assert_eq!(closed.channel_id, Some(1));
    let changed = events.next().await.unwrap();
    assert_eq!(changed.kind, v1::event::Kind::FrontendChanged as i32);
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn overloads_are_counted_and_announced() {
    let (fake, backend) = setup().await;
    let mut events = backend.events();
    let (_, _audio) = backend
        .open_channel(open_req(1, 10_000_000, v1::Mode::Am))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    fake.state().ad_over = 17;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let event = events.next().await.unwrap();
    assert_eq!(event.kind, v1::event::Kind::FrontendOverload as i32);
    let fe = backend.health().frontend.unwrap();
    assert_eq!(fe.state, v1::frontend_health::State::Overload as i32);
    assert_eq!(fe.overloads_total, 1);
    // The overload state clears after a quiet period.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let fe = backend.health().frontend.unwrap();
    assert_eq!(fe.state, v1::frontend_health::State::Ok as i32);
    assert_eq!(fe.overloads_total, 1);
}

#[tokio::test(start_paused = true)]
async fn data_group_is_discovered_from_radiod() {
    let fake = FakeRadiod::start().await;
    fake.state().data_dest = Some(local0());
    let backend = Ka9qBackend::new(config(&fake, None)).unwrap();
    // Not connected yet: health is DOWN until the first use.
    assert_eq!(
        backend.health().backend.state,
        v1::backend_health::State::Down as i32
    );
    let client = backend.client().await.unwrap();
    fake.set_rtp_dest(client.local_data_addr());
    let (_, mut audio) = backend
        .open_channel(open_req(1, 10_000_000, v1::Mode::Am))
        .await
        .unwrap();
    assert_eq!(next_audio(&mut audio).await.seq, 0);
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn discovery_failures_are_backend_errors_and_retried() {
    let fake = FakeRadiod::start().await;
    let backend = Ka9qBackend::new(config(&fake, None)).unwrap();
    // radiod does not report OUTPUT_DATA_DEST_SOCKET.
    let e = backend.capabilities().await.unwrap_err();
    assert_eq!(e.code, v1::ResultCode::BackendError);
    assert!(e.message.contains("data_group"), "{}", e.message);
    // radiod not answering at all.
    fake.state().mute = true;
    assert_eq!(
        code(backend.capabilities().await),
        v1::ResultCode::BackendError
    );
    // It comes back: the next request connects.
    {
        let mut st = fake.state();
        st.mute = false;
        st.data_dest = Some(local0());
    }
    backend.capabilities().await.unwrap();
    assert_eq!(
        backend.health().backend.state,
        v1::backend_health::State::Running as i32
    );
}

#[tokio::test(start_paused = true)]
async fn idle_radiod_is_probed_for_health() {
    let (fake, backend) = setup().await;
    assert_eq!(
        backend.health().backend.state,
        v1::backend_health::State::Down as i32,
        "nothing heard yet"
    );
    // The monitor probes a silent radiod on its own.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        backend.health().backend.state,
        v1::backend_health::State::Running as i32
    );
    let probes = fake.state().created.len();
    assert!(probes >= 1);
    // No new probe within the idle interval.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(fake.state().created.len(), probes);
    fake.assert_client_contract();
}

#[tokio::test]
async fn new_needs_a_runtime_and_a_valid_config() {
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let mut c = Ka9qConfig::new(addr, Some(local0()));
    c.client.lifetime_frames = 1;
    assert_eq!(
        Ka9qBackend::new(c).unwrap_err().code,
        v1::ResultCode::BackendError
    );
    let err =
        std::thread::spawn(move || Ka9qBackend::new(Ka9qConfig::new(addr, None)).unwrap_err())
            .join()
            .unwrap();
    assert_eq!(err.code, v1::ResultCode::BackendError);
}

#[tokio::test(start_paused = true)]
async fn measurement_channels_hop_poll_and_close() {
    use station_backend::MeasurementChannel;
    let (fake, backend) = setup().await;
    assert!(backend.can_measure());
    let caps = backend.capabilities().await.unwrap();
    let tunable = caps.receiver.tunable_range.unwrap();
    let mid = tunable.start_hz / 2 + tunable.end_hz / 2;
    let ch = MeasurementChannel {
        center_hz: mid,
        bin_count: 1024,
        resolution_bw_hz: 500.0,
        poll_interval: Duration::from_millis(500),
    };
    backend.open_measurement(7, ch).await.unwrap();
    assert_eq!(
        code(backend.open_measurement(7, ch).await),
        v1::ResultCode::AlreadyExists
    );
    assert_eq!(backend.measurement_ids(), vec![7]);
    // Not listener capacity: no channel or subscription on the hub's side.
    assert!(backend.channel_ids().is_empty() && backend.subscription_ids().is_empty());
    let ssrc = *fake.state().created.last().unwrap();
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!((fc.demod, fc.bin_count, fc.rbw), (4, 1024, 500.0));
    // SPECTRUM_AVG is sent explicitly and fixed.
    assert_eq!(fc.avg, backend_ka9q::backend::measure::SPECTRUM_AVG);
    // The placement grid from the front end's status (L + M − 1 points).
    let grid = backend.measurement_grid(500.0).await.expect("grid");
    assert!(grid.grid_hz >= 500 && grid.grid_hz % 500 == 0, "{grid:?}");
    // Narrowband bins are refused.
    let narrow = MeasurementChannel {
        resolution_bw_hz: 200.0,
        ..ch
    };
    assert_eq!(
        code(backend.open_measurement(8, narrow).await),
        v1::ResultCode::InvalidArgument
    );
    let s = backend.poll_measurement(7).await.unwrap();
    assert_eq!(s.power_db.len(), 1024);
    assert_eq!((s.center_hz, s.bin_hz), (mid, 500.0));
    assert!(s.power_db.iter().all(|p| p.is_finite()));
    // A hop changes RADIO_FREQUENCY on the same SSRC; BIN_COUNT stays.
    let hop = mid + 512_000;
    let created = fake.state().created.len();
    // The hop's own reply carries the new window.
    let reply = backend.retune_measurement(7, hop).await.unwrap();
    assert_eq!(reply.map(|s| s.center_hz), Some(hop));
    let s = backend.poll_measurement(7).await.unwrap();
    assert_eq!(s.center_hz, hop);
    let fc = fake.channel(ssrc).unwrap();
    #[allow(clippy::cast_precision_loss, reason = "a frequency below 2^53 Hz")]
    let hop_f = hop as f64;
    assert_eq!((fc.frequency, fc.bin_count), (hop_f, 1024));
    assert_eq!(fake.state().created.len(), created, "no new SSRC for a hop");
    backend.close_measurement(7).await.unwrap();
    assert!(
        fake.wait_until(Duration::from_millis(200), |s| !s
            .channels
            .contains_key(&ssrc))
            .await
    );
    assert_eq!(
        code(backend.poll_measurement(7).await),
        v1::ResultCode::NotFound
    );
    assert_eq!(
        code(backend.close_measurement(7).await),
        v1::ResultCode::NotFound
    );
    fake.assert_client_contract();
}
