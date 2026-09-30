// SPDX-License-Identifier: GPL-3.0-only
//! End to end against a real radiod: stationd (in process) with the ka9q
//! backend, talking to station-conformance's fake hub, which validates
//! every message with the §5 rules. Run by
//! `scripts/e2e-station.sh`; skipped unless `COAXHUB_E2E=1`.
//!
//! `COAXHUB_E2E_RADIOD` picks what radiod runs:
//!
//! - `siggen` (default, `testdata/radiod-siggen.conf`): 24 MS/s real,
//!   0 to 12 MHz, an AM carrier at 10 MHz modulated by 1 kHz;
//! - `rtlsdr` (`testdata/radiod-rtlsdr.conf`): an RTL-SDR at 98 MHz,
//!   2.048 MS/s; no known signal, so only rates and plumbing are checked.
//!
//! Checks: sane capabilities; a channel opens and Opus frames arrive at
//! 50/s; a retune keeps the sequence continuous; waterfall rows arrive at
//! the requested rate (and, with the signal generator, the carrier is in
//! the right bin); broadcast FM reports radiod's 48 kHz; health in
//! `StationStatus`. Timings are printed as `E2E key=value` lines.

// Static [credentials] and [backend] are development-only settings.
#![cfg(not(feature = "packaged"))]
#![allow(
    clippy::too_many_lines,
    reason = "end-to-end scripts read top to bottom"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use backend_ka9q::backend::{Ka9qBackend, Ka9qConfig};
use station_conformance::LocalPolicy;
use station_conformance::scenarios::{After, Harness, ScenarioConfig, run_scenario};
use station_conformance::{ConformanceError, StationConn};
use station_proto::v1;
use station_proto::v1::request::Command;
use station_proto::v1::response::Payload;
use station_proto::v1::station_message::Body;
use station_proto::validate::WaterfallDecoder;

const STATUS_GROUP: &str = "239.77.0.6:5006";
const SECS: u64 = 3;

fn enabled() -> bool {
    std::env::var_os("COAXHUB_E2E").is_some_and(|v| v == "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Radiod {
    SigGen,
    RtlSdr,
}

fn radiod() -> Radiod {
    match std::env::var("COAXHUB_E2E_RADIOD").as_deref() {
        Ok("rtlsdr") => Radiod::RtlSdr,
        _ => Radiod::SigGen,
    }
}

fn opus(sample_rate_hz: u32) -> v1::AudioParams {
    v1::AudioParams {
        codec: v1::AudioCodec::Opus as i32,
        bitrate_bps: 16_000,
        sample_rate_hz,
        frame_duration_ms: 20,
    }
}

fn fail(msg: String) -> ConformanceError {
    ConformanceError::Hub(msg)
}

fn check(ok: bool, msg: impl FnOnce() -> String) -> Result<(), ConformanceError> {
    if ok { Ok(()) } else { Err(fail(msg())) }
}

/// Sends `command` and returns the response and the round trip.
async fn timed(
    conn: &mut StationConn,
    command: Command,
) -> Result<(v1::Response, Duration), ConformanceError> {
    let t = Instant::now();
    let r = conn.call(command, Duration::from_secs(5)).await?;
    let code = r.result.as_ref().map_or(0, |res| res.code);
    check(code == v1::ResultCode::Ok as i32, || {
        format!("not OK: {r:?}")
    })?;
    Ok((r, t.elapsed()))
}

fn audio_seqs(msgs: &[(tokio::time::Instant, v1::StationMessage)], id: u32) -> Vec<u32> {
    msgs.iter()
        .filter_map(|(_, m)| match &m.body {
            Some(Body::Audio(a)) if a.channel_id == id => Some(a.seq),
            _ => None,
        })
        .collect()
}

fn check_rate(what: &str, n: usize, per_s: f64, secs: f64) -> Result<(), ConformanceError> {
    #[allow(clippy::cast_precision_loss, reason = "small counts")]
    let rate = n as f64 / secs;
    println!("E2E {what}_per_s={rate:.1}");
    check((rate - per_s).abs() <= per_s * 0.1, || {
        format!("{what}: {rate:.1}/s, expected {per_s}/s")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stationd_with_a_real_radiod() {
    if !enabled() {
        eprintln!("skipped: set COAXHUB_E2E=1 and run radiod (scripts/e2e-station.sh)");
        return;
    }
    let which = radiod();
    let config = Ka9qConfig::new(STATUS_GROUP.parse().expect("addr"), None);
    let backend = Arc::new(Ka9qBackend::new(config).expect("backend"));
    let mut station = common::InProcess::with_backend(backend);
    let cfg = ScenarioConfig::fast();
    let harness = Harness::new(LocalPolicy::default()).await;
    let result = run_scenario(&mut station, &cfg, harness, async |h, _sut| {
        let mut conn = h.connect(cfg.connect_within, &cfg).await?;

        // Capabilities.
        let (r, rtt) = timed(&mut conn, Command::GetCapabilities(v1::GetCapabilities {})).await?;
        println!("E2E get_capabilities_ms={}", rtt.as_millis());
        let Some(Payload::Capabilities(caps)) = r.payload else {
            return Err(fail(format!("no capabilities: {r:?}")));
        };
        let range = caps
            .tunable_range
            .ok_or_else(|| fail("no tunable range".into()))?;
        println!(
            "E2E tunable_range={}..{} sample_rate={} sdr_model={:?}",
            range.start_hz, range.end_hz, caps.sample_rate_hz, caps.sdr_model
        );
        let (center, mode) = match which {
            Radiod::SigGen => {
                check(range.start_hz == 0 && range.end_hz == 12_000_000, || {
                    format!("signal generator range {range:?}")
                })?;
                check(caps.sample_rate_hz == 24_000_000, || "sample rate".into())?;
                (10_000_000, v1::Mode::Am)
            }
            Radiod::RtlSdr => {
                check(
                    range.start_hz < 98_000_000 && range.end_hz > 98_000_000,
                    || format!("RTL-SDR range {range:?} does not contain 98 MHz"),
                )?;
                (98_000_000, v1::Mode::Nfm)
            }
        };
        check(caps.modes.len() == 6, || "modes".into())?;

        // A channel at 50 frames/s.
        let open = Command::OpenChannel(v1::OpenChannel {
            channel_id: 1,
            frequency_hz: center,
            mode: mode as i32,
            low_cut_hz: -5000,
            high_cut_hz: 5000,
            audio: Some(opus(12_000)),
            squelch_snr_db: None,
        });
        let (r, rtt) = timed(&mut conn, open).await?;
        println!("E2E open_channel_ms={}", rtt.as_millis());
        let Some(Payload::ChannelOpened(opened)) = r.payload else {
            return Err(fail(format!("no ChannelOpened: {r:?}")));
        };
        println!("E2E channel_opened={opened:?}");
        let _ = conn.collect(Duration::from_millis(300)).await?;
        let before = conn.collect(Duration::from_secs(SECS)).await?;
        let seqs = audio_seqs(&before, 1);
        #[allow(clippy::cast_precision_loss, reason = "small")]
        check_rate("audio_frames", seqs.len(), 50.0, SECS as f64)?;
        let signals: Vec<_> = before
            .iter()
            .filter_map(|(_, m)| match &m.body {
                Some(Body::Signal(s)) => Some(*s),
                _ => None,
            })
            .collect();
        if let Some(s) = signals.last() {
            println!(
                "E2E signal_snr_db={:.1} signal_power_dbfs={:.1} signals={}",
                s.snr_db,
                s.power_dbfs,
                signals.len()
            );
        }
        check(!signals.is_empty(), || "no ChannelSignal".into())?;
        if which == Radiod::SigGen {
            let snr = signals.last().map_or(0.0, |s| s.snr_db);
            check(snr > 20.0, || format!("AM carrier SNR {snr} dB"))?;
        }

        // Retunes keep the sequence continuous; time the round trips.
        let mut rtts = Vec::new();
        for (i, offset) in [1_000_i64, -2_000, 500, 0].iter().enumerate() {
            let f = center.checked_add_signed(*offset).unwrap_or(center);
            let (r, rtt) = timed(
                &mut conn,
                Command::UpdateChannel(v1::UpdateChannel {
                    channel_id: 1,
                    frequency_hz: Some(f),
                    ..Default::default()
                }),
            )
            .await?;
            let Some(Payload::ChannelUpdated(u)) = r.payload else {
                return Err(fail(format!("no ChannelUpdated: {r:?}")));
            };
            check(u.frequency_hz == f, || format!("retune {i}: {u:?}"))?;
            rtts.push(rtt.as_millis());
        }
        println!("E2E update_channel_ms={rtts:?}");
        let after = conn.collect(Duration::from_secs(1)).await?;
        let mut all = before;
        all.extend(after);
        let seqs = audio_seqs(&all, 1);
        let gaps: Vec<_> = seqs
            .windows(2)
            .filter(|w| w[1] != w[0].wrapping_add(1))
            .collect();
        println!("E2E audio_seq_gaps={}", gaps.len());
        check(gaps.is_empty(), || {
            format!("sequence gaps across retunes: {gaps:?}")
        })?;

        // Broadcast FM: radiod runs WFM at 48 kHz whatever was asked, and
        // squelch off keeps its (always squelched) demodulator open.
        let wfm = Command::OpenChannel(v1::OpenChannel {
            channel_id: 2,
            frequency_hz: center,
            mode: v1::Mode::Fm as i32,
            low_cut_hz: 0,
            high_cut_hz: 0,
            audio: Some(opus(24_000)),
            squelch_snr_db: None,
        });
        let (r, _) = timed(&mut conn, wfm).await?;
        let Some(Payload::ChannelOpened(opened)) = r.payload else {
            return Err(fail(format!("no ChannelOpened: {r:?}")));
        };
        let rate = opened.audio.map_or(0, |a| a.sample_rate_hz);
        println!("E2E wfm_sample_rate={rate}");
        check(rate == 48_000, || format!("WFM runs at {rate} Hz"))?;
        let got = conn.collect(Duration::from_secs(2)).await?;
        let n = audio_seqs(&got, 2).len();
        println!("E2E wfm_frames_2s={n}");
        check(n >= 80, || {
            format!("only {n} WFM frames in 2 s with squelch off")
        })?;
        timed(
            &mut conn,
            Command::CloseChannel(v1::CloseChannel { channel_id: 2 }),
        )
        .await?;

        // The full-span waterfall, as the hub subscribes it.
        let bins = 1024;
        let sub = Command::SubscribeWaterfall(v1::SubscribeWaterfall {
            subscription_id: 7,
            range: Some(range),
            bins,
            row_interval_ms: 100,
            quantization: None,
            compression: v1::WaterfallCompression::Zstd as i32,
        });
        let (r, rtt) = timed(&mut conn, sub).await?;
        println!("E2E subscribe_waterfall_ms={}", rtt.as_millis());
        let Some(Payload::WaterfallSubscribed(w)) = r.payload else {
            return Err(fail(format!("no WaterfallSubscribed: {r:?}")));
        };
        println!("E2E waterfall_subscribed={w:?}");
        let _ = conn.collect(Duration::from_millis(500)).await?;
        let got = conn.collect(Duration::from_secs(SECS)).await?;
        let mut decoder = WaterfallDecoder::new();
        let mut row = Vec::new();
        let mut rows = 0;
        let mut compressed = 0;
        let mut peaks = Vec::new();
        let mut floors = Vec::new();
        for (_, m) in &got {
            if let Some(Body::Waterfall(wr)) = &m.body {
                decoder
                    .decode(&wr.payload, w.compression, w.bins, &mut row)
                    .map_err(|e| fail(format!("row: {e}")))?;
                rows += 1;
                compressed += wr.payload.len();
                let peak = row
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, v)| **v)
                    .map_or(0, |(i, _)| i);
                peaks.push(peak);
                let mut sorted = row.clone();
                sorted.sort_unstable();
                floors.push(sorted[sorted.len() / 2]);
            }
        }
        #[allow(clippy::cast_precision_loss, reason = "small")]
        check_rate("waterfall_rows", rows, 10.0, SECS as f64)?;
        let floor = floors.get(floors.len() / 2).copied().unwrap_or(0);
        println!(
            "E2E waterfall_bytes_per_row={} floor_db_hz={:.1}",
            compressed / rows.max(1),
            -130.0 + 0.5 * f64::from(floor)
        );
        if which == Radiod::SigGen {
            let wr = w.range.unwrap_or(range);
            #[allow(clippy::cast_precision_loss, reason = "at most 6 GHz")]
            let rbw = (wr.end_hz - wr.start_hz) as f64 / f64::from(w.bins);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "a bin index"
            )]
            let expected = ((10e6 - wr.start_hz as f64) / rbw).floor() as usize;
            println!("E2E carrier_bin_expected={expected} peaks={peaks:?}");
            check(peaks.iter().all(|p| p.abs_diff(expected) <= 1), || {
                format!("carrier not in bin {expected}: {peaks:?}")
            })?;
        }

        // Health in StationStatus.
        let status = got
            .iter()
            .rev()
            .find_map(|(_, m)| match &m.body {
                Some(Body::Status(s)) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| fail("no StationStatus".into()))?;
        println!("E2E station_status={status:?}");
        check(
            status.backend.as_ref().map(|b| b.state)
                == Some(v1::backend_health::State::Running as i32),
            || "backend not RUNNING".into(),
        )?;
        check(status.frontend.is_some(), || "no front-end health".into())?;
        Ok(After::Stop(Some(Box::new(conn))))
    })
    .await;
    if let Err(e) = result {
        panic!("e2e against radiod ({which:?}): {e}");
    }
}
