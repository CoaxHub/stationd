// SPDX-License-Identifier: GPL-3.0-only
//! Hermetic integration tests of the radiod client against a fake radiod on
//! 127.0.0.1 (see `support/mod.rs`). No real radiod, no multicast.
//!
//! Long-running scenarios use Tokio's paused clock: the fake's 20 ms frame
//! ticker and the client's timers advance in virtual time, and loopback UDP
//! is delivered before the clock moves, so ten seconds of radiod time take
//! milliseconds.

#![allow(clippy::float_cmp, reason = "frequencies are copied, not computed")]

mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use backend_ka9q::client::{
    AudioChannelParams, AudioUpdate, ClientConfig, ClientError, Ka9qClient, SpectrumParams,
    SpectrumUpdate,
};
use backend_ka9q::command::Command;
use backend_ka9q::spectrum::{DEFAULT_QUANTIZATION, requantize};
use backend_ka9q::tags::{PktType, StatusType};
use backend_ka9q::tlv::Encoder;
use support::{FakeRadiod, fake_scale};

const LIFETIME_FRAMES: u32 = 50; // 1 s

fn config(fake: &FakeRadiod) -> ClientConfig {
    ClientConfig {
        status_addr: fake.addr,
        data_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        command_timeout: Duration::from_millis(100),
        command_retries: 2,
        lifetime_frames: LIFETIME_FRAMES,
        ..ClientConfig::default()
    }
}

async fn setup() -> (FakeRadiod, Ka9qClient) {
    let fake = FakeRadiod::start().await;
    let client = Ka9qClient::connect(config(&fake)).unwrap();
    fake.set_rtp_dest(client.local_data_addr());
    (fake, client)
}

fn am(freq: f64) -> AudioChannelParams {
    AudioChannelParams {
        samprate: Some(12_000),
        opus_bitrate: Some(16_000),
        ..AudioChannelParams::new(freq, "am")
    }
}

#[tokio::test(start_paused = true)]
async fn open_receive_close_audio_channel() {
    let (fake, client) = setup().await;
    let mut ch = client.open_audio_channel(am(10e6)).await.unwrap();
    let ssrc = ch.ssrc();
    assert!(ssrc != 0 && ssrc != u32::MAX);

    let fc = fake.channel(ssrc).unwrap();
    assert_eq!(fc.frequency, 10e6);
    assert_eq!(fc.encoding, 3);
    assert_eq!(fc.samprate, 12_000);
    assert_eq!(fc.opus_bitrate, 16_000);
    assert_eq!(fc.lifetime_set, LIFETIME_FRAMES);
    assert_eq!(fc.preset, "am");

    let status = ch.status().unwrap();
    assert_eq!(status.radio_frequency_hz, Some(10e6));
    assert_eq!(ch.params(), am(10e6));

    for expect_seq in 0..10 {
        let p = ch.recv().await.unwrap();
        assert_eq!(p.seq, expect_seq);
        assert_eq!(p.timestamp, expect_seq * 960);
        assert_eq!(p.lost_before, 0);
        assert_eq!(p.payload_type, 111);
        assert_eq!(p.payload[0], 0x28);
        assert_eq!(p.payload[1], ssrc.to_be_bytes()[3]);
    }
    assert!(ch.stats().rtp_packets >= 10);

    ch.close().await.unwrap();
    assert!(
        fake.wait_until(Duration::from_millis(100), |s| !s
            .channels
            .contains_key(&ssrc))
            .await,
        "channel still exists after close"
    );
    let st = fake.state();
    let last = st.log.last().unwrap();
    assert_eq!(last.ssrc, ssrc);
    assert_eq!(last.lifetime, Some(1), "close must send LIFETIME 1");
    assert_eq!(last.frequency, Some(0.0));
    drop(st);
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn keepalive_keeps_the_channel_alive_and_drop_destroys_it() {
    let (fake, client) = setup().await;
    let ch = client.open_audio_channel(am(7.1e6)).await.unwrap();
    let ssrc = ch.ssrc();
    // Five lifetimes without any user command.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(fake.has_channel(ssrc), "channel expired despite keepalive");
    let stats = ch.stats();
    assert!(stats.keepalives >= 12, "{stats:?}");
    assert_eq!(stats.keepalive_failures, 0);
    assert!(!fake.state().removed.contains(&ssrc));

    drop(ch);
    assert!(
        fake.wait_until(Duration::from_millis(200), |s| !s
            .channels
            .contains_key(&ssrc))
            .await
    );
    // Nothing re-creates it afterwards.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!fake.has_channel(ssrc));
    fake.assert_client_contract();
}

/// The lifetime semantics the keepalive exists for: a channel that is not
/// refreshed disappears after `LIFETIME` frames (checked on the fake, with a
/// raw command, to keep the fake honest).
#[tokio::test(start_paused = true)]
async fn unrefreshed_channel_expires() {
    let fake = FakeRadiod::start().await;
    let sock = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    sock.send_to(&Command::poll(1234, 1).lifetime(10).encode(), fake.addr)
        .await
        .unwrap();
    assert!(
        fake.wait_until(Duration::from_millis(100), |s| s
            .channels
            .contains_key(&1234))
            .await
    );
    assert!(
        fake.wait_until(Duration::from_millis(400), |s| s.removed.contains(&1234))
            .await
    );
    // Without LIFETIME the channel would be immortal.
    sock.send_to(&Command::poll(5678, 1).encode(), fake.addr)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(fake.has_channel(5678));
}

#[tokio::test(start_paused = true)]
async fn lost_replies_are_retried_then_time_out() {
    let (fake, client) = setup().await;

    // One lost reply: the retry succeeds.
    fake.state().drop_replies = 1;
    let ch = client.open_audio_channel(am(10e6)).await.unwrap();
    assert_eq!(client.stats().command_timeouts, 1);
    let creates = fake
        .state()
        .log
        .iter()
        .filter(|c| c.ssrc == ch.ssrc())
        .count();
    assert_eq!(creates, 2);
    ch.close().await.unwrap();

    // No replies at all: typed timeout after 1 + retries attempts, and the
    // client still tries to destroy what it may have created.
    fake.state().mute = true;
    let err = client.open_audio_channel(am(10e6)).await.unwrap_err();
    let ClientError::Timeout { ssrc, attempts } = err else {
        panic!("expected a timeout, got {err}");
    };
    assert_eq!(attempts, 3);
    let st = fake.state();
    let sent: Vec<_> = st.log.iter().filter(|c| c.ssrc == ssrc).collect();
    assert_eq!(sent.len(), 6, "3 creates + 3 destroys: {sent:?}");
    assert!(sent[3..].iter().all(|c| c.lifetime == Some(1)));
    drop(st);
    // The destroys were executed even though they were not answered.
    assert!(!fake.has_channel(ssrc));
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn rejected_open_is_reported_and_cleaned_up() {
    let (fake, client) = setup().await;
    fake.state().ignore_frequency = true;
    let err = client.open_audio_channel(am(10e6)).await.unwrap_err();
    let ClientError::Rejected { ssrc, reason } = err else {
        panic!("expected a rejection, got {err}");
    };
    assert!(reason.contains("RADIO_FREQUENCY"), "{reason}");
    assert!(
        fake.wait_until(Duration::from_millis(100), |s| !s
            .channels
            .contains_key(&ssrc))
            .await
    );
    // Invalid parameters never reach radiod.
    let before = fake.state().log.len();
    let err = client.open_audio_channel(am(-1.0)).await.unwrap_err();
    assert!(matches!(err, ClientError::InvalidParams(_)));
    assert_eq!(fake.state().log.len(), before);
}

#[tokio::test(start_paused = true)]
async fn commands_are_serialized_per_ssrc() {
    let (fake, client) = setup().await;
    let ch = Arc::new(client.open_audio_channel(am(10e6)).await.unwrap());
    let mut tasks = Vec::new();
    for i in 0..12 {
        let ch = Arc::clone(&ch);
        tasks.push(tokio::spawn(async move {
            ch.update(AudioUpdate {
                frequency_hz: Some(10e6 + f64::from(i) * 1000.0),
                ..AudioUpdate::default()
            })
            .await
        }));
    }
    for t in tasks {
        let st = t.await.unwrap().unwrap();
        assert!(st.radio_frequency_hz.unwrap() >= 10e6);
    }
    // The keepalive runs concurrently too.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let ssrc = ch.ssrc();
    assert_eq!(fake.state().max_queue_depth.get(&ssrc), Some(&1));
    assert!(ch.params().frequency_hz >= 10e6);
    fake.assert_client_contract();

    // Invalid updates are rejected locally.
    let err = ch
        .update(AudioUpdate {
            low_edge_hz: Some(6000.0),
            high_edge_hz: Some(-6000.0),
            ..AudioUpdate::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::InvalidParams(_)));
    // A full update is applied.
    let st = ch
        .update(AudioUpdate {
            frequency_hz: Some(9e6),
            low_edge_hz: Some(-3000.0),
            high_edge_hz: Some(3000.0),
            opus_bitrate: Some(24_000),
            ..AudioUpdate::default()
        })
        .await
        .unwrap();
    assert_eq!(st.radio_frequency_hz, Some(9e6));
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!(
        (fc.low_edge, fc.high_edge, fc.opus_bitrate),
        (-3000.0, 3000.0, 24_000)
    );
}

#[tokio::test(start_paused = true)]
async fn rtp_is_demultiplexed_by_ssrc() {
    let (fake, client) = setup().await;
    let mut a = client.open_audio_channel(am(10e6)).await.unwrap();
    let mut b = client.open_audio_channel(am(5e6)).await.unwrap();
    assert_ne!(a.ssrc(), b.ssrc());
    // A foreign channel (created by "someone else") also sends RTP.
    let other = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let foreign = Command::new(4242, 1)
        .preset("am")
        .output_encoding(backend_ka9q::tags::Encoding::Opus)
        .radio_frequency_hz(1e6)
        .lifetime(500)
        .encode();
    other.send_to(&foreign, fake.addr).await.unwrap();

    for i in 0..20 {
        let pa = a.recv().await.unwrap();
        let pb = b.recv().await.unwrap();
        assert_eq!((pa.seq, pb.seq), (i, i));
        assert_eq!(pa.payload[1], a.ssrc().to_be_bytes()[3]);
        assert_eq!(pb.payload[1], b.ssrc().to_be_bytes()[3]);
    }
    assert!(client.stats().rtp_foreign > 0);
    // The foreign channel's status reply went to `other`, not to us.
    a.close().await.unwrap();
    // After close the queue ends.
    assert!(b.recv().await.is_some());
    b.close().await.unwrap();
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn sequence_tracking_reports_loss_duplicates_and_reordering() {
    let (fake, client) = setup().await;
    {
        let mut st = fake.state();
        st.rtp_drop_every = 10;
        st.rtp_duplicate_every = 7;
        st.rtp_swap_every = 13;
    }
    let mut ch = client.open_audio_channel(am(10e6)).await.unwrap();
    let mut packets = Vec::new();
    while packets.len() < 100 {
        packets.push(ch.recv().await.unwrap());
    }
    let mut lost_total = 0u64;
    for w in packets.windows(2) {
        assert_eq!(w[1].seq, w[0].seq + 1, "station seq is contiguous");
        let gap = 1 + w[1].lost_before;
        assert_eq!(w[1].timestamp - w[0].timestamp, gap * 960);
        lost_total += u64::from(w[1].lost_before);
    }
    lost_total += u64::from(packets[0].lost_before);
    let stats = ch.stats();
    assert!(lost_total >= 9, "lost {lost_total}");
    assert!(stats.duplicates_or_late >= 10, "{stats:?}");
    // Everything lost so far was either dropped or overtaken.
    assert!(stats.lost >= lost_total);
    assert_eq!(stats.queue_drops, 0);
    ch.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn slow_consumer_drops_oldest_packets() {
    let fake = FakeRadiod::start().await;
    let client = Ka9qClient::connect(ClientConfig {
        audio_queue_len: 5,
        ..config(&fake)
    })
    .unwrap();
    fake.set_rtp_dest(client.local_data_addr());
    let mut ch = client.open_audio_channel(am(10e6)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let stats = ch.stats();
    assert!(stats.queue_drops >= 40, "{stats:?}");
    // The oldest were dropped: what is left is recent and contiguous.
    let first = ch.try_recv().unwrap();
    assert!(first.seq >= 40);
    let second = ch.try_recv().unwrap();
    assert_eq!(second.seq, first.seq + 1);
    ch.close().await.unwrap();
}

/// radiod overrides the output rate for some modes (WFM runs at 48 kHz). The
/// client must adopt it: re-sending the requested rate at every keepalive
/// restarts the demodulator each time (seen on a real RTL-SDR).
#[tokio::test(start_paused = true)]
async fn radiod_sample_rate_override_does_not_restart_the_channel() {
    let (fake, client) = setup().await;
    let params = AudioChannelParams {
        samprate: Some(24_000),
        ..AudioChannelParams::new(97.5e6, "wfm")
    };
    let ch = client.open_audio_channel(params).await.unwrap();
    let ssrc = ch.ssrc();
    let restarts_after_open = fake.channel(ssrc).unwrap().restarts;
    // Many keepalives.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let fc = fake.channel(ssrc).unwrap();
    let stats = ch.stats();
    assert!(stats.keepalives >= 12, "{stats:?}");
    assert_eq!(stats.recreations, 0, "{stats:?}");
    assert_eq!(fc.samprate, 48_000);
    assert_eq!(
        fc.restarts, restarts_after_open,
        "keepalives restarted the channel"
    );
    assert_eq!(ch.params().samprate, Some(48_000));
    ch.close().await.unwrap();
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn lost_audio_channel_is_recreated() {
    let (fake, client) = setup().await;
    let ch = client.open_audio_channel(am(10e6)).await.unwrap();
    let ssrc = ch.ssrc();
    fake.expire(ssrc);
    // The next keepalive re-creates it from the template (0 Hz, PCM); the
    // client notices and re-applies its parameters.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!(fc.frequency, 10e6);
    assert_eq!(fc.encoding, 3);
    assert_eq!(fc.lifetime_set, LIFETIME_FRAMES);
    assert_eq!(ch.stats().recreations, 1);
    ch.close().await.unwrap();
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn spectrum_rows_arrive_at_the_poll_rate() {
    let (fake, client) = setup().await;
    let params = SpectrumParams::new(10e6, 1024, 100.0);
    let mut sp = client.open_spectrum_channel(params.clone()).await.unwrap();
    let ssrc = sp.ssrc();
    assert_eq!(sp.params().await, params);
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!((fc.demod, fc.bin_count, fc.rbw), (4, 1024, 100.0));

    let start = tokio::time::Instant::now();
    let q = DEFAULT_QUANTIZATION;
    let mut rows = 0;
    while start.elapsed() < Duration::from_secs(10) {
        let frame = sp.recv().await.unwrap();
        rows += 1;
        assert_eq!(frame.bins.len(), 1024);
        assert_eq!(frame.center_hz, 10e6);
        assert_eq!(frame.bin_width_hz, 100.0);
        let peak = frame
            .bins
            .iter()
            .enumerate()
            .max_by_key(|(_, v)| **v)
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(peak, 512);
        assert_eq!(frame.frequency_of_bin(peak), 10e6);
        // Each row is decoded with its own packet's scale.
        let valid_scales: Vec<(f32, f32)> = (0..12).map(fake_scale).collect();
        assert!(valid_scales.contains(&(frame.base_db, frame.step_db)));
        let ours = requantize(&frame, &q);
        // Normalised to dB/Hz: 100 Hz bins -> subtract 20 dB.
        let want = q.quantize(frame.base_db + 255.0 * frame.step_db - 20.0);
        assert_eq!(ours[512], want);
    }
    assert!((98..=101).contains(&rows), "{rows} rows in 10 s");
    let stats = sp.stats();
    assert!(stats.empty_replies <= 1, "{stats:?}");
    assert_eq!(stats.poll_failures, 0);
    assert!(sp.status().unwrap().bin_bytes.is_some());
    sp.close().await.unwrap();
    assert!(
        fake.wait_until(Duration::from_millis(100), |s| !s
            .channels
            .contains_key(&ssrc))
            .await
    );
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn bin_count_change_uses_a_new_ssrc() {
    let (fake, client) = setup().await;
    let mut sp = client
        .open_spectrum_channel(SpectrumParams::new(10e6, 512, 1000.0))
        .await
        .unwrap();
    let old = sp.ssrc();
    assert_eq!(sp.recv().await.unwrap().bins.len(), 512);

    // Same bin count: in place.
    let same = sp
        .update(SpectrumUpdate {
            center_hz: Some(10.1e6),
            resolution_bw_hz: Some(500.0),
            avg: Some(4),
            ..SpectrumUpdate::default()
        })
        .await
        .unwrap();
    assert_eq!(same, old);
    let fc = fake.channel(old).unwrap();
    assert_eq!((fc.frequency, fc.rbw, fc.avg), (10.1e6, 500.0, 4));

    // New bin count: new SSRC, old channel destroyed, radiod not crashed.
    let new = sp
        .update(SpectrumUpdate {
            bin_count: Some(2048),
            ..SpectrumUpdate::default()
        })
        .await
        .unwrap();
    assert_ne!(new, old);
    assert_eq!(sp.ssrc(), new);
    assert!(!fake.has_channel(old));
    assert_eq!(fake.channel(new).unwrap().bin_count, 2048);
    loop {
        let f = sp.recv().await.unwrap();
        if f.bins.len() == 2048 {
            assert_eq!(f.center_hz, 10.1e6);
            break;
        }
    }
    assert_eq!(sp.stats().geometry_changes, 1);
    assert_eq!(sp.params().await.bin_count, 2048);

    // Invalid: rejected locally, nothing changes.
    let err = sp
        .update(SpectrumUpdate {
            bin_count: Some(0),
            ..SpectrumUpdate::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::InvalidParams(_)));
    assert_eq!(sp.ssrc(), new);
    fake.assert_client_contract();
    drop(sp);
    assert!(
        fake.wait_until(Duration::from_millis(100), |s| !s
            .channels
            .contains_key(&new))
            .await
    );
}

#[tokio::test(start_paused = true)]
async fn lost_spectrum_channel_is_recreated() {
    let (fake, client) = setup().await;
    let mut sp = client
        .open_spectrum_channel(SpectrumParams::new(10e6, 256, 1000.0))
        .await
        .unwrap();
    let ssrc = sp.ssrc();
    sp.recv().await.unwrap();
    fake.expire(ssrc);
    // The next poll re-creates a template (linear) channel; the client sees
    // DEMOD_TYPE 0 and re-sends the spectrum create command.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let fc = fake.channel(ssrc).unwrap();
    assert_eq!((fc.demod, fc.bin_count), (4, 256));
    assert_eq!(sp.stats().recreations, 1);
    while sp.try_recv().is_some() {}
    let f = sp.recv().await.unwrap();
    assert_eq!(f.bins.len(), 256);
    fake.assert_client_contract();
}

#[tokio::test(start_paused = true)]
async fn spectrum_poll_timeouts_are_counted() {
    let (fake, client) = setup().await;
    let mut sp = client
        .open_spectrum_channel(SpectrumParams::new(10e6, 64, 1000.0))
        .await
        .unwrap();
    sp.recv().await.unwrap();
    fake.state().mute = true;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(sp.stats().poll_failures >= 1, "{:?}", sp.stats());
    fake.state().mute = false;
    // Polling resumes.
    while sp.try_recv().is_some() {}
    sp.recv().await.unwrap();
    // A failed open of a spectrum channel is typed too.
    fake.state().mute = true;
    let err = client
        .open_spectrum_channel(SpectrumParams::new(10e6, 64, 1000.0))
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Timeout { attempts: 3, .. }));
    let err = client
        .open_spectrum_channel(SpectrumParams::new(10e6, 0, 1000.0))
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::InvalidParams(_)));
}

#[tokio::test(start_paused = true)]
async fn garbage_on_the_status_socket_is_ignored() {
    let (fake, client) = setup().await;
    let to = client.local_status_addr();
    let garbage: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xff],
        vec![0, 18, 0x85, 1, 2],                // bad length-of-length
        vec![0, 18, 50, 1],                     // length past the end
        vec![1, 18, 1, 5, 0],                   // a CMD packet
        Encoder::new(PktType::Status).finish(), // no SSRC
        Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 99)
            .bytes(StatusType::RadioFrequency, &[0; 9]) // 9-byte double
            .finish(),
        (0..=255u8).collect(),
    ];
    for g in &garbage {
        fake.send_raw(g, to).await;
    }
    // A foreign but valid status.
    fake.send_raw(
        &Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 77)
            .finish(),
        to,
    )
    .await;
    // Garbage on the data socket too.
    fake.send_raw(&[0x80, 111, 0], client.local_data_addr())
        .await;
    fake.send_raw(&[0x40; 20], client.local_data_addr()).await;

    let ch = client.open_audio_channel(am(10e6)).await.unwrap();
    let stats = client.stats();
    assert_eq!(stats.garbage_status, 7, "{stats:?}");
    assert_eq!(stats.command_packets_seen, 1);
    assert!(stats.foreign_status >= 1);
    assert_eq!(stats.rtp_garbage, 2);
    assert!(stats.commands_sent >= 1);
    ch.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dump_all_and_probe() {
    let (fake, client) = setup().await;
    // No channels: nothing comes back.
    assert!(
        client
            .dump_all(Duration::from_millis(100))
            .await
            .unwrap()
            .is_empty()
    );

    let a = client.open_audio_channel(am(10e6)).await.unwrap();
    let sp = client
        .open_spectrum_channel(SpectrumParams::new(10e6, 128, 1000.0))
        .await
        .unwrap();
    let all = client.dump_all(Duration::from_millis(200)).await.unwrap();
    let ssrcs: Vec<u32> = all.iter().filter_map(|s| s.ssrc).collect();
    assert!(
        ssrcs.contains(&a.ssrc()) && ssrcs.contains(&sp.ssrc()),
        "{ssrcs:?}"
    );
    // The all-channels request carried no LIFETIME and created nothing.
    assert!(!fake.has_channel(u32::MAX));

    let before: Vec<u32> = fake.state().channels.keys().copied().collect();
    let st = client.probe().await.unwrap();
    assert_eq!(st.input_samprate, Some(support::INPUT_SAMPRATE));
    let probe_ssrc = st.ssrc.unwrap();
    assert!(!before.contains(&probe_ssrc));
    assert!(
        fake.wait_until(Duration::from_millis(100), |s| !s
            .channels
            .contains_key(&probe_ssrc))
            .await
    );
    fake.assert_client_contract();
    a.close().await.unwrap();
    sp.close().await.unwrap();
}

#[tokio::test]
async fn connect_rejects_bad_config_and_works_in_real_time() {
    let fake = FakeRadiod::start().await;
    let err = Ka9qClient::connect(ClientConfig {
        lifetime_frames: 0,
        ..config(&fake)
    })
    .unwrap_err();
    assert!(matches!(err, ClientError::InvalidConfig(_)));
    let client = Ka9qClient::connect(config(&fake)).unwrap();
    assert_eq!(client.config().status_addr, fake.addr);
    fake.set_rtp_dest(client.local_data_addr());
    // Real clock: a short end-to-end run.
    let mut ch = client.clone().open_audio_channel(am(10e6)).await.unwrap();
    let p = tokio::time::timeout(Duration::from_secs(2), ch.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.seq, 0);
    ch.close().await.unwrap();
}

#[test]
fn connect_outside_a_runtime_is_an_error() {
    let err = Ka9qClient::connect(ClientConfig::default()).unwrap_err();
    assert!(matches!(err, ClientError::Net(_)), "{err}");
}
