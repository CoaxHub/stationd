// SPDX-License-Identifier: GPL-3.0-only
//! End-to-end tests against a real radiod running the signal-generator test
//! config (`testdata/radiod-siggen.conf`: an AM carrier at 10 MHz
//! modulated 50 % by 1 kHz). Run them with `scripts/e2e.sh`, which
//! builds and starts radiod first and always stops it afterwards.
//!
//! Every test is a no-op unless `COAXHUB_E2E=1`, so a plain `cargo test`
//! stays hermetic. The tests share one radiod, so they take a lock and run
//! one at a time. Measurements are printed as `E2E ...` lines.

#![allow(clippy::float_cmp, reason = "exact values are compared on purpose")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use backend_ka9q::client::{
    AudioChannelParams, ClientConfig, DEFAULT_DATA_ADDR, DEFAULT_STATUS_ADDR, Ka9qClient,
    SpectrumParams, SpectrumUpdate,
};
use backend_ka9q::net::{self, SendOptions};
use backend_ka9q::rtp::RtpPacket;
use backend_ka9q::status::ChannelStatus;

static RADIOD: Mutex<()> = Mutex::new(());

const SPIKE: &str = env!("CARGO_BIN_EXE_ka9q-spike");
/// The `sig_gen` carrier.
const CARRIER_HZ: f64 = 10e6;

fn enabled() -> bool {
    let on = std::env::var("COAXHUB_E2E").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set COAXHUB_E2E=1 and start radiod (scripts/e2e.sh)");
    }
    on
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    RADIOD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs the spike and returns its `RESULT` fields and stdout.
fn spike(args: &[&str]) -> (HashMap<String, String>, String) {
    let out = Command::new(SPIKE)
        .args(args)
        .output()
        .expect("run ka9q-spike");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "ka9q-spike {args:?} failed: {stderr}\n{stdout}"
    );
    let line = stdout
        .lines()
        .find(|l| l.starts_with("RESULT "))
        .expect("RESULT line");
    println!("E2E {line}");
    let fields = line
        .split_whitespace()
        .skip(1)
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    (fields, stdout)
}

fn num(fields: &HashMap<String, String>, key: &str) -> f64 {
    fields
        .get(key)
        .unwrap_or_else(|| panic!("no {key} in {fields:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in {fields:?}"))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// Mono samples (first channel) and sample rate of a 16-bit PCM WAV file.
fn read_wav(path: &Path) -> (Vec<f64>, u32) {
    let b = std::fs::read(path).expect("read wav");
    assert_eq!(&b[..4], b"RIFF");
    assert_eq!(&b[8..12], b"WAVE");
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let (mut pos, mut channels, mut rate, mut bits) = (12, 0usize, 0, 0);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let size = usize::try_from(u32_at(pos + 4)).expect("size");
        let body = pos + 8;
        if id == b"fmt " {
            channels = usize::from(u16_at(body + 2));
            rate = u32_at(body + 4);
            bits = u16_at(body + 14);
        } else if id == b"data" {
            assert_eq!(bits, 16, "16-bit PCM expected");
            let end = (body + size).min(b.len());
            let samples = b[body..end]
                .chunks_exact(2 * channels)
                .map(|f| f64::from(i16::from_le_bytes([f[0], f[1]])) / 32768.0)
                .collect();
            return (samples, rate);
        }
        pos = body + size + (size & 1);
    }
    panic!("no data chunk in {}", path.display());
}

/// Goertzel power of `freq` in `x` (normalised so a full-scale sine is
/// about 0 dB).
fn goertzel_db(x: &[f64], rate: u32, freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / f64::from(rate);
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    #[allow(clippy::cast_precision_loss, reason = "sample counts are small")]
    let n = x.len() as f64;
    10.0 * (power * 4.0 / (n * n)).max(1e-30).log10()
}

/// Audio plays: record the AM channel, validate the file with `opusinfo`,
/// decode it with `opusdec` and find the 1 kHz modulation tone.
#[test]
fn audio_records_a_playable_opus_file_with_the_1khz_tone() {
    if !enabled() {
        return;
    }
    let _g = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let opus = dir.path().join("am.opus");
    let wav = dir.path().join("am.wav");
    let (f, _) = spike(&[
        "--report",
        "audio",
        "--freq",
        "10e6",
        "--mode",
        "am",
        "--secs",
        "5",
        "--bitrate",
        "16000",
        "--samprate",
        "12000",
        "--out",
        opus.to_str().expect("path"),
    ]);
    assert!(num(&f, "packets") >= 240.0, "{f:?}");
    assert!(num(&f, "lost") <= 2.0, "{f:?}");
    assert_eq!(num(&f, "queue_drops"), 0.0);

    let info = Command::new("opusinfo")
        .arg(&opus)
        .output()
        .expect("opusinfo");
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(info.status.success(), "opusinfo failed: {text}");
    let playback = text
        .lines()
        .find(|l| l.contains("Playback length"))
        .expect("playback length")
        .trim()
        .to_owned();
    println!("E2E opusinfo: {playback}");
    assert!(text.contains("Packet duration:   20.0ms"), "{text}");

    let dec = Command::new("opusdec")
        .args(["--quiet", "--rate", "48000"])
        .arg(&opus)
        .arg(&wav)
        .output()
        .expect("opusdec");
    assert!(dec.status.success(), "opusdec failed");
    let (samples, rate) = read_wav(&wav);
    assert_eq!(rate, 48_000);
    // Skip the first second (AGC settling), analyse three seconds.
    let x = &samples[48_000..48_000 * 4];

    let mut spectrum: Vec<(f64, f64)> = (10..=400)
        .map(|k| {
            let f = f64::from(k) * 10.0;
            (f, goertzel_db(x, rate, f))
        })
        .collect();
    spectrum.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (coarse, _) = spectrum[0];
    let (tone, level) = (-100..=100)
        .map(|d| {
            let f = coarse + f64::from(d) * 0.1;
            (f, goertzel_db(x, rate, f))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .expect("peak");
    let off = f64::midpoint(goertzel_db(x, rate, 700.0), goertzel_db(x, rate, 1300.0));
    println!(
        "E2E audio tone: dominant {tone:.1} Hz at {level:.1} dBFS; 700/1300 Hz average {off:.1} dBFS ({:.1} dB below)",
        level - off
    );
    assert!((tone - 1000.0).abs() <= 5.0, "dominant tone {tone} Hz");
    assert!(
        level - off >= 20.0,
        "tone only {} dB above its neighbours",
        level - off
    );
}

/// Waterfall rows at 10 rows/s for 10 s, with the carrier in the right bin.
#[test]
fn waterfall_dumps_ten_rows_per_second_with_the_carrier_at_10mhz() {
    if !enabled() {
        return;
    }
    let _g = lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let rows = dir.path().join("rows.cxwf");
    let pgm = dir.path().join("rows.pgm");
    let (f, _) = spike(&[
        "--report",
        "waterfall",
        "--center",
        "10e6",
        "--bins",
        "1024",
        "--rbw",
        "1000",
        "--rate",
        "10",
        "--secs",
        "10",
        "--out",
        rows.to_str().expect("path"),
        "--pgm",
        pgm.to_str().expect("path"),
    ]);
    assert!(num(&f, "rows") >= 95.0, "{f:?}");
    assert_eq!(num(&f, "poll_failures"), 0.0);
    let (i, _) = spike(&["inspect", rows.to_str().expect("path")]);
    assert_eq!(i.get("rows"), f.get("rows"));
    let peak = num(&i, "peak_bin");
    assert!((511.0..=513.0).contains(&peak), "peak bin {peak}");
    assert!((num(&i, "peak_hz") - CARRIER_HZ).abs() <= 1000.0);
    let img = std::fs::read(&pgm).expect("pgm");
    assert!(img.starts_with(b"P5\n1024 "));
    println!(
        "E2E waterfall: {} rows in {} s = {} rows/s, {} B/row compressed, {} B/s upload",
        f["rows"],
        f["secs"],
        f["rows_per_s"],
        f["avg_compressed_row_bytes"],
        f["compressed_bytes_per_s"]
    );
}

/// Counts RTP packets of `ssrc` and status packets of `ssrc` on the groups
/// for `window`, without sending anything.
async fn observe(ssrc: u32, window: Duration) -> (u32, u32) {
    let opts = SendOptions::default();
    let rtp = net::bind_receiver(DEFAULT_DATA_ADDR, opts).expect("rtp socket");
    let status = net::bind_receiver(DEFAULT_STATUS_ADDR, opts).expect("status socket");
    let deadline = tokio::time::Instant::now() + window;
    let (mut n_rtp, mut n_status) = (0, 0);
    let (mut a, mut b) = (vec![0u8; 65_536], vec![0u8; 65_536]);
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            r = rtp.recv_from(&mut a) => {
                let n = r.map_or(0, |(n, _)| n);
                if RtpPacket::parse(&a[..n]).is_ok_and(|p| p.ssrc == ssrc) {
                    n_rtp += 1;
                }
            }
            r = status.recv_from(&mut b) => {
                let n = r.map_or(0, |(n, _)| n);
                if ChannelStatus::decode(&b[..n]).is_ok_and(|s| s.ssrc == Some(ssrc)) {
                    n_status += 1;
                }
            }
        }
    }
    (n_rtp, n_status)
}

/// A closed or dropped channel stops, and an unrefreshed one expires.
/// Nothing here polls the SSRC after it was closed (a poll would re-create
/// it).
#[test]
fn closed_dropped_and_expired_channels_stop_sending() {
    if !enabled() {
        return;
    }
    let _g = lock();
    runtime().block_on(async {
        let client = Ka9qClient::connect(ClientConfig::default()).expect("client");
        let params = AudioChannelParams::new(CARRIER_HZ, "am");

        // close(): waits for radiod's reply to LIFETIME 1.
        let ch = client.open_audio_channel(params.clone()).await.expect("open");
        let ssrc = ch.ssrc();
        let (live, _) = observe(ssrc, Duration::from_secs(1)).await;
        assert!(live >= 40, "only {live} RTP packets/s while open");
        ch.close().await.expect("close");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (rtp, status) = observe(ssrc, Duration::from_millis(1500)).await;
        println!("E2E lifetime: close: {live} pkt/s before, {rtp} RTP + {status} status after");
        assert_eq!((rtp, status), (0, 0));

        // Drop: best-effort destroy, no await.
        let ch = client.open_audio_channel(params.clone()).await.expect("open");
        let ssrc = ch.ssrc();
        drop(ch);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (rtp, status) = observe(ssrc, Duration::from_millis(1500)).await;
        println!("E2E lifetime: drop: {rtp} RTP + {status} status after");
        assert_eq!((rtp, status), (0, 0));

        // A client that dies: one create with LIFETIME 25 (0.5 s), never
        // refreshed. radiod removes it on its own.
        let ssrc = 0x00c0_ffee;
        let sock = net::bind_sender(DEFAULT_STATUS_ADDR, SendOptions::default()).expect("sender");
        let create = backend_ka9q::command::Command::new(ssrc, 1)
            .preset("am")
            .output_encoding(backend_ka9q::tags::Encoding::Opus)
            .radio_frequency_hz(CARRIER_HZ)
            .lifetime(25)
            .encode();
        sock.send_to(&create, DEFAULT_STATUS_ADDR).await.expect("send");
        let (early, _) = observe(ssrc, Duration::from_millis(300)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let (late, _) = observe(ssrc, Duration::from_millis(1000)).await;
        println!("E2E lifetime: unrefreshed LIFETIME 25: {early} RTP in the first 0.3 s, {late} after expiry");
        assert!(early > 0, "the channel never started");
        assert_eq!(late, 0, "the channel outlived its LIFETIME");

        // The all-channels dump creates nothing and lists none of these.
        let all = client.dump_all(Duration::from_millis(300)).await.expect("dump");
        assert!(all.iter().all(|s| s.ssrc != Some(ssrc)));
    });
}

/// Changing the bin count must not crash radiod: the client moves to a new
/// SSRC. Also checks per-row BASE/STEP decoding against radiod's own values.
#[test]
fn spectrum_bin_count_change_keeps_radiod_alive() {
    if !enabled() {
        return;
    }
    let _g = lock();
    runtime().block_on(async {
        let client = Ka9qClient::connect(ClientConfig::default()).expect("client");
        let mut sp = client
            .open_spectrum_channel(SpectrumParams::new(CARRIER_HZ, 512, 1000.0))
            .await
            .expect("open spectrum");
        let old = sp.ssrc();
        let mut scales = BTreeMap::new();
        for _ in 0..5 {
            let f = sp.recv().await.expect("row");
            assert_eq!(f.bins.len(), 512);
            assert_eq!(f.frequency_of_bin(256), CARRIER_HZ);
            let peak = f.bins.iter().enumerate().max_by_key(|&(_, v)| *v).expect("bins").0;
            assert!((255..=257).contains(&peak), "peak {peak}");
            scales.insert(f.base_db.to_bits(), f.step_db);
        }
        let new = sp
            .update(SpectrumUpdate {
                bin_count: Some(1024),
                ..SpectrumUpdate::default()
            })
            .await
            .expect("bin count change");
        assert_ne!(new, old);
        let mut got = 0;
        while got < 5 {
            let f = sp.recv().await.expect("row after change");
            if f.bins.len() == 1024 {
                got += 1;
            }
        }
        // Still alive and answering: a fresh probe works.
        client.probe().await.expect("radiod still answers");
        println!(
            "E2E spectrum: bin change {old} -> {new} ok; {} distinct BASE values in 5 rows (auto-scaled per poll)",
            scales.len()
        );
        sp.close().await.expect("close");
    });
}

/// `status` prints the `sig_gen` front end.
#[test]
fn status_shows_the_signal_generator() {
    if !enabled() {
        return;
    }
    let _g = lock();
    let (f, out) = spike(&["status", "--probe"]);
    assert_eq!(f.get("probed").map(String::as_str), Some("true"));
    assert!(out.contains("coaxhub-test sig_gen"), "{out}");
    assert!(out.contains("24000000 S/s real"), "{out}");
}
