// SPDX-License-Identifier: GPL-3.0-only
//! Pure mappings between station-protocol values and radiod's (station-
//! protocol §9): modes to presets, front-end status to capabilities,
//! waterfall ranges to spectrum2 geometry, channel status to S-meter
//! readings. No I/O; every function is total on untrusted input.

use station_backend::{BackendCapabilities, BackendError};
use station_proto::{limits, v1};

use super::placement::{self, FrontEnd, Placement};
use crate::client::Squelch;
use crate::spectrum;
use crate::status::ChannelStatus;

/// Modes this backend offers, in `Capabilities.modes` order.
pub const MODES: [v1::Mode; 6] = [
    v1::Mode::Am,
    v1::Mode::Fm,
    v1::Mode::Nfm,
    v1::Mode::Usb,
    v1::Mode::Lsb,
    v1::Mode::Cw,
];

/// Most simultaneous audio channels offered on one radiod (the protocol
/// cap; stationd's claim per receiver, station protocol §7, limits it
/// further).
pub const MAX_CHANNELS: u32 = limits::MAX_CHANNELS;
/// Most bins per waterfall row. A noisy row of 8192 bins still compresses
/// well below the 16 KiB row limit (§5).
pub const MAX_BINS: u32 = 8192;
/// Fastest row rate: radiod answers about one spectrum poll per two 20 ms
/// frames, and 20 rows/s was measured clean (radiod-notes.md §6).
pub const MIN_ROW_INTERVAL_MS: u32 = 50;
/// Most simultaneous waterfall subscriptions offered.
pub const MAX_SUBSCRIPTIONS: u32 = 8;
/// Opus frame durations radiod can produce: one frame per radiod block.
const FRAME_TOLERANCE_MS: f64 = 0.5;
/// SNR squelch hysteresis: close this many dB below the open threshold
/// (radiod's defaults are open 8 dB, close 7 dB, `defaults.h`).
pub const SQUELCH_HYSTERESIS_DB: f32 = 1.0;
/// Reported SNRs are clamped to this range, dB.
const SNR_LIMIT_DB: f64 = 100.0;

/// The radiod preset for `mode` (the pinned `presets.conf`):
///
/// | Mode | Preset | radiod demodulator |
/// |---|---|---|
/// | AM | `am` | linear, envelope, 12 kHz, ±5 kHz |
/// | FM (broadcast) | `wfm` | wfm, always 48 kHz output |
/// | NFM | `fm` | fm, 24 kHz, ±8 kHz, no de-emphasis |
/// | USB | `usb` | linear, 12 kHz, +50..+3000 Hz |
/// | LSB | `lsb` | linear, 12 kHz, −3000..−50 Hz |
/// | CW | `cwu` | linear, 12 kHz, ±200 Hz, 500 Hz tone |
///
/// `UNSPECIFIED` is `INVALID_ARGUMENT`; an unknown value (a newer minor)
/// is `UNSUPPORTED` (§3).
pub fn preset_for(mode: i32) -> Result<&'static str, BackendError> {
    match v1::Mode::try_from(mode) {
        Ok(v1::Mode::Unspecified) => Err(BackendError::invalid_argument("mode is unspecified")),
        Ok(v1::Mode::Am) => Ok("am"),
        Ok(v1::Mode::Fm) => Ok("wfm"),
        Ok(v1::Mode::Nfm) => Ok("fm"),
        Ok(v1::Mode::Usb) => Ok("usb"),
        Ok(v1::Mode::Lsb) => Ok("lsb"),
        Ok(v1::Mode::Cw) => Ok("cwu"),
        Err(_) => Err(BackendError::unsupported("mode not supported")),
    }
}

/// Checks the hub's audio parameters (§4): Opus only, and
/// every value from the protocol's allowed sets.
pub fn check_audio(audio: Option<v1::AudioParams>) -> Result<v1::AudioParams, BackendError> {
    let audio =
        audio.ok_or_else(|| BackendError::invalid_argument("audio parameters are missing"))?;
    if audio.codec != v1::AudioCodec::Opus as i32 {
        return Err(BackendError::unsupported("codec not supported (Opus only)"));
    }
    if !limits::AUDIO_BITRATE_BPS.contains(&audio.bitrate_bps)
        || !limits::AUDIO_SAMPLE_RATES_HZ.contains(&audio.sample_rate_hz)
        || !limits::AUDIO_FRAME_DURATIONS_MS.contains(&audio.frame_duration_ms)
    {
        return Err(BackendError::invalid_argument(
            "bitrate, sample rate or frame duration not allowed",
        ));
    }
    Ok(audio)
}

/// radiod's `wfm` demodulator always outputs 48 kHz (`wfm.c` sets
/// `output.samprate = Audio_samprate`), whatever was requested.
pub const WFM_SAMPLE_RATE_HZ: u32 = 48_000;

/// The Opus sample rate a new channel actually runs at. radiod applies the
/// `wfm` override only when the demodulator restarts, after its reply to
/// the creating command, so that reply still shows the requested rate:
/// `wfm` is therefore 48 kHz regardless. Otherwise radiod's reported
/// `OUTPUT_SAMPRATE` if it is an allowed rate, else the requested one.
pub fn actual_sample_rate(preset: &str, requested: u32, reported: Option<u32>) -> u32 {
    if preset == "wfm" {
        return WFM_SAMPLE_RATE_HZ;
    }
    reported
        .filter(|r| limits::AUDIO_SAMPLE_RATES_HZ.contains(r))
        .unwrap_or(requested)
}

/// The Opus frame duration radiod produces, in ms, from its block time, if
/// it is one the protocol allows (10, 20, 40 or 60 ms).
pub fn frame_duration_ms(frame: std::time::Duration) -> Option<u32> {
    let ms = frame.as_secs_f64() * 1000.0;
    limits::AUDIO_FRAME_DURATIONS_MS
        .iter()
        .copied()
        .find(|&d| (f64::from(d) - ms).abs() <= FRAME_TOLERANCE_MS)
}

/// Passband edges for an `OpenChannel`: `(0, 0)` keeps the preset's,
/// anything else must have `low < high`.
pub fn open_edges(low: i32, high: i32) -> Result<(Option<f32>, Option<f32>), BackendError> {
    if low == 0 && high == 0 {
        return Ok((None, None));
    }
    if low >= high {
        return Err(BackendError::invalid_argument(
            "low_cut_hz must be below high_cut_hz",
        ));
    }
    Ok((Some(edge(low)), Some(edge(high))))
}

/// A hub filter edge (Hz, ±2^31) as radiod's float.
#[allow(
    clippy::cast_precision_loss,
    reason = "filter edges are at most a few hundred kHz"
)]
pub fn edge(hz: i32) -> f32 {
    hz as f32
}

/// The squelch for `OpenChannel.squelch_snr_db` (absent = off).
pub fn open_squelch(snr_db: Option<f32>) -> Result<Squelch, BackendError> {
    match snr_db {
        None => Ok(Squelch::Off),
        Some(db) => snr_squelch(db),
    }
}

/// The squelch change of an `UpdateChannel`, if any.
pub fn update_squelch(snr_db: Option<f32>, off: bool) -> Result<Option<Squelch>, BackendError> {
    if off {
        return Ok(Some(Squelch::Off));
    }
    snr_db.map(snr_squelch).transpose()
}

fn snr_squelch(db: f32) -> Result<Squelch, BackendError> {
    if !db.is_finite() || db.abs() > 100.0 {
        return Err(BackendError::invalid_argument(
            "squelch_snr_db must be finite, within ±100 dB",
        ));
    }
    Ok(Squelch::Snr {
        open_db: db,
        close_db: db - SQUELCH_HYSTERESIS_DB,
    })
}

/// A radiod frequency (Hz, `f64`) as a protocol frequency, if it is finite,
/// non-negative and at most 6 GHz.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "checked to be within 0..=6e9 first"
)]
pub fn hz(f: f64) -> Option<u64> {
    let r = f.round();
    #[allow(clippy::cast_precision_loss, reason = "6e9 is exact in f64")]
    let max = limits::MAX_FREQUENCY_HZ as f64;
    (r.is_finite() && (0.0..=max).contains(&r)).then_some(r as u64)
}

/// A protocol frequency as radiod's `f64` (exact up to 2^53 Hz).
#[allow(clippy::cast_precision_loss, reason = "frequencies are at most 6 GHz")]
pub fn freq(hz: u64) -> f64 {
    hz as f64
}

/// A radiod edge (Hz) rounded to the protocol's `i32`, if finite.
#[allow(
    clippy::cast_possible_truncation,
    reason = "clamped to the i32 range first"
)]
pub fn edge_hz(f: f64) -> Option<i32> {
    f.is_finite()
        .then(|| f.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
}

/// The range the front end covers now: `FIRST_LO_FREQUENCY` plus the front
/// end's filter edges `FE_LOW_EDGE`..`FE_HIGH_EDGE`, or ± half the sample
/// rate when the edges are missing. Clipped to 0..=6 GHz. `None` when the
/// status has no front-end information or the range is empty.
pub fn tunable_range(st: &ChannelStatus) -> Option<v1::FrequencyRange> {
    let lo = st.first_lo_hz.filter(|f| f.is_finite()).unwrap_or(0.0);
    let (low, high) = match (st.fe_low_edge_hz, st.fe_high_edge_hz) {
        (Some(l), Some(h)) if l.is_finite() && h.is_finite() && l < h => (l, h),
        _ => {
            let rate = f64::from(st.input_samprate.filter(|r| *r > 0)?);
            if st.fe_isreal == Some(true) {
                (0.0, rate / 2.0)
            } else {
                (-rate / 2.0, rate / 2.0)
            }
        }
    };
    #[allow(clippy::cast_precision_loss, reason = "6e9 is exact in f64")]
    let max = limits::MAX_FREQUENCY_HZ as f64;
    let start = hz((lo + low).clamp(0.0, max))?;
    let end = hz((lo + high).clamp(0.0, max))?;
    (start < end).then_some(v1::FrequencyRange {
        start_hz: start,
        end_hz: end,
    })
}

/// Whether `hz` lies in `range` (inclusive).
pub fn contains(range: &v1::FrequencyRange, hz: u64) -> bool {
    (range.start_hz..=range.end_hz).contains(&hz)
}

/// The receiver's capabilities from its radiod's status (§9
/// `GetCapabilities`).
///
/// radiod does not report its tuner's full range or whether its driver has
/// runtime gain or attenuation hooks, so `hardware_range` is the current
/// tunable range (stationd widens it to the receiver's profile) and the
/// front-end controls offer only a retune, the one change the pinned
/// `rtlsdr` and `sig_gen` drivers take live (§9 `SetFrontend`).
pub fn capabilities(st: &ChannelStatus) -> Option<BackendCapabilities> {
    let tunable = tunable_range(st)?;
    Some(BackendCapabilities {
        receiver: v1::ReceiverCapabilities {
            receiver_id: 0,
            tunable_range: Some(tunable),
            hardware_range: Some(tunable),
            sample_rate_hz: st.input_samprate.unwrap_or(0),
            max_channels: MAX_CHANNELS,
            frontend: Some(v1::FrontendControls {
                can_retune: true,
                ..v1::FrontendControls::default()
            }),
            sdr_model: sdr_model(st),
        },
        modes: MODES.iter().map(|m| *m as i32).collect(),
        codecs: vec![v1::AudioCodec::Opus as i32],
        waterfall: Some(v1::WaterfallLimits {
            max_bins: MAX_BINS,
            min_row_interval_ms: MIN_ROW_INTERVAL_MS,
            max_subscriptions: MAX_SUBSCRIPTIONS,
        }),
    })
}

/// radiod's front-end `DESCRIPTION`, cut to the protocol's 64 bytes on a
/// character boundary, with control characters removed. Untrusted: the
/// hub escapes it.
pub fn sdr_model(st: &ChannelStatus) -> String {
    let mut out = String::new();
    for c in st
        .description
        .as_deref()
        .unwrap_or("ka9q-radio")
        .chars()
        .filter(|c| !c.is_control())
    {
        if out.len() + c.len_utf8() > limits::MAX_SDR_MODEL_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// The front-end state for `SetFrontend` replies (`receiver_id` 0:
/// stationd sets it).
#[allow(clippy::cast_possible_truncation, reason = "gains are small dB values")]
pub fn frontend_state(st: &ChannelStatus) -> v1::FrontendState {
    let tunable = tunable_range(st);
    let finite = |v: Option<f64>| v.filter(|x| x.is_finite()).unwrap_or(0.0) as f32;
    v1::FrontendState {
        receiver_id: 0,
        gain_db: finite(st.rf_gain_db),
        hardware_agc: st.rf_agc.unwrap_or(false),
        attenuation_db: finite(st.rf_atten_db),
        antenna_port: String::new(),
        center_frequency_hz: tunable.map_or(0, |r| midpoint(&r)),
        tunable_range: tunable,
    }
}

/// The middle of `r`.
pub fn midpoint(r: &v1::FrequencyRange) -> u64 {
    r.start_hz + (r.end_hz - r.start_hz) / 2
}

/// The spectrum2 channel for a waterfall range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// `RADIO_FREQUENCY`, Hz.
    pub center_hz: f64,
    /// `RESOLUTION_BW`: bin width, Hz.
    pub rbw_hz: f32,
    /// `BIN_COUNT`.
    pub bins: u32,
    /// Where radiod's bins will be: the low edge of bin 0 to the high edge
    /// of the last bin, which `bins` equal bins tile.
    pub covered: v1::FrequencyRange,
}

/// The spectrum2 geometry for `bins` bins across `range`
/// (radiod-notes.md §10 "Waterfall geometry"):
///
/// - bins wider than radiod's crossover (wideband mode) with a known front
///   end: [`placement::plan_wideband`], whose real bins lie inside the
///   range and cover all of it but less than two bins;
/// - otherwise (narrowband mode, which radiod places exactly): bin width
///   `(end − start) / bins` and the centre bin (`⌈bins/2⌉`) placed so that
///   the bins tile the range.
pub fn geometry(
    range: &v1::FrequencyRange,
    bins: u32,
    fe: Option<&FrontEnd>,
) -> Result<Geometry, BackendError> {
    if range.start_hz >= range.end_hz {
        return Err(BackendError::invalid_argument("range is empty or reversed"));
    }
    if bins == 0 {
        return Err(BackendError::invalid_argument("bins must be positive"));
    }
    let (start, end) = (freq(range.start_hz), freq(range.end_hz));
    let rbw = (end - start) / f64::from(bins);
    if rbw > placement::CROSSOVER_HZ {
        if let Some(w) = fe.and_then(|fe| placement::plan_wideband(fe, start, end, bins)) {
            if let Some(covered) = edges(&w.placement) {
                return Ok(Geometry {
                    center_hz: w.center_hz,
                    rbw_hz: w.rbw_hz,
                    bins,
                    covered,
                });
            }
        }
    }
    let center = start + (f64::from(placement::center_index(bins)) + 0.5) * rbw;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "radiod's RESOLUTION_BW is a C float"
    )]
    let rbw_hz = rbw as f32;
    Ok(Geometry {
        center_hz: center,
        rbw_hz,
        bins,
        covered: *range,
    })
}

/// The range a spectrum2 channel really covers, from its status: where
/// radiod puts `bins` bins asked for at `RADIO_FREQUENCY` with
/// `RESOLUTION_BW` ([`placement::placement`]; wideband mode needs the
/// front-end fields). `None` when a field is missing or the range leaves
/// 0..=6 GHz.
pub fn covered_range(st: &ChannelStatus, bins: u32) -> Option<v1::FrequencyRange> {
    let rf = st.radio_frequency_hz?;
    let rbw = st.resolution_bw_hz?;
    edges(&placement::placement(
        FrontEnd::from_status(st).as_ref(),
        rf,
        rbw,
        bins,
    )?)
}

/// The edges of `p` in whole Hz, if within 0..=6 GHz and not empty.
fn edges(p: &Placement) -> Option<v1::FrequencyRange> {
    #[allow(clippy::cast_precision_loss, reason = "6e9 is exact in f64")]
    let max = limits::MAX_FREQUENCY_HZ as f64;
    let (low, high) = (p.low_edge(), p.high_edge());
    if !((0.0..=max).contains(&low.round()) && (0.0..=max).contains(&high.round())) {
        return None;
    }
    let range = v1::FrequencyRange {
        start_hz: hz(low)?,
        end_hz: hz(high)?,
    };
    (range.start_hz < range.end_hz).then_some(range)
}

/// The requested quantization, or the default, if valid.
pub fn quantization(q: Option<v1::Quantization>) -> Result<v1::Quantization, BackendError> {
    let q = q.unwrap_or(v1::Quantization {
        offset_db: spectrum::DEFAULT_QUANTIZATION.offset_db,
        step_db: spectrum::DEFAULT_QUANTIZATION.step_db,
    });
    if q.offset_db.is_finite() && q.step_db.is_finite() && q.step_db > 0.0 {
        Ok(q)
    } else {
        Err(BackendError::invalid_argument("invalid quantization"))
    }
}

/// The channel's SNR in dB, as radiod's `tune` computes it: `FM_SNR` for
/// FM channels, `PLL_SNR` for a PLL, otherwise
/// `BASEBAND_POWER / (NOISE_DENSITY × bandwidth) − 1`. Clamped to ±100 dB.
pub fn snr_db(st: &ChannelStatus) -> Option<f64> {
    let snr = st
        .fm_snr_db
        .or(st.pll_snr_db)
        .filter(|v| v.is_finite())
        .or_else(|| estimated_snr_db(st))?;
    snr.is_finite()
        .then(|| snr.clamp(-SNR_LIMIT_DB, SNR_LIMIT_DB))
}

/// `BASEBAND_POWER / (NOISE_DENSITY × bandwidth) − 1`, in dB.
fn estimated_snr_db(st: &ChannelStatus) -> Option<f64> {
    let power = st.baseband_power_db.filter(|v| v.is_finite())?;
    let n0 = st.noise_density_db.filter(|v| v.is_finite())?;
    let bw = (st.high_edge_hz? - st.low_edge_hz?).abs();
    if !bw.is_finite() || bw <= 0.0 {
        return None;
    }
    let noise = 10f64.powf(n0 / 10.0) * bw;
    let ratio = 10f64.powf(power / 10.0) / noise - 1.0;
    Some(if ratio > 0.0 {
        10.0 * ratio.log10()
    } else {
        -SNR_LIMIT_DB
    })
}

/// An S-meter reading for `channel_id` from a channel status (§9
/// `ChannelSignal`): the SNR ([`snr_db`]) and `BASEBAND_POWER` as the
/// power in the channel filter. `squelch_open_db` is the channel's squelch
/// threshold, if its squelch is on. `None` if the status lacks either
/// value or they are not finite.
#[allow(
    clippy::cast_possible_truncation,
    reason = "dB values within ±200 fit f32"
)]
pub fn channel_signal(
    channel_id: u32,
    st: &ChannelStatus,
    squelch_open_db: Option<f32>,
) -> Option<v1::ChannelSignal> {
    let snr = snr_db(st)?;
    let power = st
        .baseband_power_db
        .filter(|v| v.is_finite())?
        .clamp(-200.0, 200.0);
    Some(v1::ChannelSignal {
        channel_id,
        snr_db: snr as f32,
        power_dbfs: power as f32,
        squelch_open: squelch_open_db.map(|open| snr >= f64::from(open)),
    })
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "values are exact")]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Duration;

    fn siggen() -> ChannelStatus {
        ChannelStatus {
            input_samprate: Some(24_000_000),
            fe_isreal: Some(true),
            fe_low_edge_hz: Some(0.0),
            fe_high_edge_hz: Some(12e6),
            first_lo_hz: Some(0.0),
            description: Some("coaxhub-test sig_gen".into()),
            ..ChannelStatus::default()
        }
    }

    #[test]
    fn presets_follow_the_table() {
        let cases = [
            (v1::Mode::Am, "am"),
            (v1::Mode::Fm, "wfm"),
            (v1::Mode::Nfm, "fm"),
            (v1::Mode::Usb, "usb"),
            (v1::Mode::Lsb, "lsb"),
            (v1::Mode::Cw, "cwu"),
        ];
        for (mode, preset) in cases {
            assert_eq!(preset_for(mode as i32).unwrap(), preset);
        }
        assert_eq!(
            preset_for(0).unwrap_err().code,
            v1::ResultCode::InvalidArgument
        );
        assert_eq!(
            preset_for(99).unwrap_err().code,
            v1::ResultCode::Unsupported
        );
        assert_eq!(MODES.len(), cases.len());
    }

    #[test]
    fn audio_parameters() {
        let ok = v1::AudioParams {
            codec: v1::AudioCodec::Opus as i32,
            bitrate_bps: 16_000,
            sample_rate_hz: 12_000,
            frame_duration_ms: 20,
        };
        assert_eq!(check_audio(Some(ok)).unwrap(), ok);
        let code = |a| check_audio(a).unwrap_err().code;
        assert_eq!(code(None), v1::ResultCode::InvalidArgument);
        assert_eq!(
            code(Some(v1::AudioParams { codec: 9, ..ok })),
            v1::ResultCode::Unsupported
        );
        for bad in [
            v1::AudioParams {
                bitrate_bps: 1,
                ..ok
            },
            v1::AudioParams {
                sample_rate_hz: 44_100,
                ..ok
            },
            v1::AudioParams {
                frame_duration_ms: 25,
                ..ok
            },
        ] {
            assert_eq!(code(Some(bad)), v1::ResultCode::InvalidArgument);
        }
        assert_eq!(actual_sample_rate("wfm", 24_000, Some(24_000)), 48_000);
        assert_eq!(actual_sample_rate("am", 12_000, Some(16_000)), 16_000);
        assert_eq!(actual_sample_rate("am", 12_000, Some(11_025)), 12_000);
        assert_eq!(actual_sample_rate("am", 12_000, None), 12_000);
        assert_eq!(frame_duration_ms(Duration::from_millis(20)), Some(20));
        assert_eq!(frame_duration_ms(Duration::from_micros(20_300)), Some(20));
        assert_eq!(frame_duration_ms(Duration::from_millis(25)), None);
    }

    #[test]
    fn edges_and_squelch() {
        assert_eq!(open_edges(0, 0).unwrap(), (None, None));
        assert_eq!(
            open_edges(-3000, 3000).unwrap(),
            (Some(-3000.0), Some(3000.0))
        );
        assert!(open_edges(100, 100).is_err());
        assert!(open_edges(100, -100).is_err());
        assert_eq!(open_squelch(None).unwrap(), Squelch::Off);
        assert_eq!(
            open_squelch(Some(10.0)).unwrap(),
            Squelch::Snr {
                open_db: 10.0,
                close_db: 9.0
            }
        );
        assert!(open_squelch(Some(f32::NAN)).is_err());
        assert!(open_squelch(Some(500.0)).is_err());
        assert_eq!(update_squelch(Some(3.0), true).unwrap(), Some(Squelch::Off));
        assert_eq!(update_squelch(None, false).unwrap(), None);
        assert!(update_squelch(Some(f32::INFINITY), false).is_err());
        assert_eq!(edge_hz(-2999.6), Some(-3000));
        assert_eq!(edge_hz(f64::NAN), None);
        assert_eq!(edge_hz(1e12), Some(i32::MAX));
    }

    #[test]
    fn frequency_conversions() {
        assert_eq!(hz(10e6), Some(10_000_000));
        assert_eq!(hz(10e6 + 0.4), Some(10_000_000));
        assert_eq!(hz(-1.0), None);
        assert_eq!(hz(7e9), None);
        assert_eq!(hz(f64::NAN), None);
        assert_eq!(freq(145_500_000), 145.5e6);
    }

    #[test]
    fn tunable_range_of_real_and_complex_front_ends() {
        let r = tunable_range(&siggen()).unwrap();
        assert_eq!((r.start_hz, r.end_hz), (0, 12_000_000));
        // An RTL-SDR at 7.1 MHz, 1.8 MS/s complex.
        let rtl = ChannelStatus {
            input_samprate: Some(1_800_000),
            fe_isreal: Some(false),
            fe_low_edge_hz: Some(-850_000.0),
            fe_high_edge_hz: Some(850_000.0),
            first_lo_hz: Some(7.1e6),
            ..ChannelStatus::default()
        };
        let r = tunable_range(&rtl).unwrap();
        assert_eq!((r.start_hz, r.end_hz), (6_250_000, 7_950_000));
        // No edges: ± half the sample rate (0..rate/2 for a real one).
        let no_edges = ChannelStatus {
            fe_low_edge_hz: None,
            ..rtl.clone()
        };
        let r = tunable_range(&no_edges).unwrap();
        assert_eq!((r.start_hz, r.end_hz), (6_200_000, 8_000_000));
        let real = ChannelStatus {
            fe_low_edge_hz: None,
            ..siggen()
        };
        assert_eq!(tunable_range(&real).unwrap().end_hz, 12_000_000);
        // Clipped at 0 Hz; nothing without front-end fields.
        let low = ChannelStatus {
            first_lo_hz: Some(100_000.0),
            ..rtl
        };
        assert_eq!(tunable_range(&low).unwrap().start_hz, 0);
        assert_eq!(tunable_range(&ChannelStatus::default()), None);
    }

    #[test]
    fn capabilities_from_the_signal_generator_pass_hub_validation() {
        let caps = capabilities(&siggen()).unwrap();
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
        let sanitized = station_proto::validate::sanitize_capabilities(&station, 0).unwrap();
        assert!(sanitized.clamped.is_empty(), "{:?}", sanitized.clamped);
        assert_eq!(caps.receiver.sample_rate_hz, 24_000_000);
        assert_eq!(caps.receiver.sdr_model, "coaxhub-test sig_gen");
        assert!(
            caps.receiver
                .frontend
                .is_some_and(|f| f.can_retune && !f.has_gain)
        );
        assert_eq!(caps.modes.len(), 6);
        let fe = frontend_state(&siggen());
        assert_eq!(fe.center_frequency_hz, 6_000_000);
        assert_eq!(fe.gain_db, 0.0);
        assert!(capabilities(&ChannelStatus::default()).is_none());
    }

    #[test]
    fn sdr_model_is_bounded_and_clean() {
        let st = ChannelStatus {
            description: Some(format!("a\u{1b}[31m{}", "é".repeat(40))),
            ..ChannelStatus::default()
        };
        let m = sdr_model(&st);
        assert!(m.len() <= limits::MAX_SDR_MODEL_BYTES);
        assert!(!m.contains('\u{1b}'));
        assert!(m.starts_with("a[31m"));
        assert_eq!(sdr_model(&ChannelStatus::default()), "ka9q-radio");
    }

    /// The front end of [`siggen`].
    fn siggen_fe() -> FrontEnd {
        FrontEnd::from_status(&ChannelStatus {
            filter_blocksize: Some(480_000),
            filter_fir_length: Some(120_001),
            ..siggen()
        })
        .unwrap()
    }

    /// What radiod reports for a spectrum channel of `g` on [`siggen`].
    fn spectrum_status(g: &Geometry) -> ChannelStatus {
        ChannelStatus {
            radio_frequency_hz: Some(g.center_hz),
            resolution_bw_hz: Some(f64::from(g.rbw_hz)),
            filter_blocksize: Some(480_000),
            filter_fir_length: Some(120_001),
            ..siggen()
        }
    }

    #[test]
    fn wideband_geometry_follows_radiod() {
        let range = v1::FrequencyRange {
            start_hz: 0,
            end_hz: 12_000_000,
        };
        let fe = siggen_fe();
        let g = geometry(&range, 1024, Some(&fe)).unwrap();
        assert_eq!(g.bins, 1024);
        // 11,718.75 Hz bins would need the bin at DC (half below 0 Hz) or
        // past Nyquist; 2050 points (11,707.3 Hz) fit inside.
        assert_eq!(fe.fft_n(f64::from(g.rbw_hz)), Some(2050));
        assert!(g.covered.start_hz > 0 && g.covered.end_hz <= 12_000_000);
        assert_eq!(covered_range(&spectrum_status(&g), 1024), Some(g.covered));
        // The labels a listener sees (the covered range in equal bins) are
        // radiod's bins within a hertz, and the 10 MHz carrier is in its bin.
        let p = placement::placement(Some(&fe), g.center_hz, f64::from(g.rbw_hz), 1024).unwrap();
        let width = (freq(g.covered.end_hz) - freq(g.covered.start_hz)) / 1024.0;
        let label = |i: u32| freq(g.covered.start_hz) + (f64::from(i) + 0.5) * width;
        for i in 0..1024 {
            assert!((label(i) - p.bin_center(i)).abs() < 1.0, "bin {i}");
        }
        let carrier = (0..1024)
            .find(|&i| (label(i) - 10e6).abs() <= width / 2.0)
            .unwrap();
        assert!((p.bin_center(carrier) - 10e6).abs() <= p.bin_hz / 2.0);
        // Without the front end: the nominal request, as asked.
        let nominal = geometry(&range, 4096, None).unwrap();
        assert_eq!(nominal.rbw_hz, 2929.6875);
        assert_eq!(nominal.center_hz, (2048.0 + 0.5) * 2929.6875);
        assert_eq!(nominal.covered, range);
        assert!(geometry(&v1::FrequencyRange::default(), 64, Some(&fe)).is_err());
        assert!(geometry(&range, 0, Some(&fe)).is_err());
    }

    /// The RTL-SDR of radiod-notes.md §10: the full span in 1024 bins.
    #[test]
    fn the_rtlsdr_full_span() {
        let rtl = ChannelStatus {
            input_samprate: Some(2_048_000),
            fe_isreal: Some(false),
            fe_low_edge_hz: Some(-962_560.0),
            fe_high_edge_hz: Some(962_560.0),
            first_lo_hz: Some(7.1e6),
            filter_blocksize: Some(40_960),
            filter_fir_length: Some(10_241),
            ..ChannelStatus::default()
        };
        let range = tunable_range(&rtl).unwrap();
        assert_eq!((range.start_hz, range.end_hz), (6_137_440, 8_062_560));
        let fe = FrontEnd::from_status(&rtl).unwrap();
        let g = geometry(&range, 1024, Some(&fe)).unwrap();
        let st = ChannelStatus {
            radio_frequency_hz: Some(g.center_hz),
            resolution_bw_hz: Some(f64::from(g.rbw_hz)),
            ..rtl.clone()
        };
        assert_eq!(covered_range(&st, 1024), Some(g.covered));
        assert!(g.covered.start_hz >= range.start_hz && g.covered.end_hz <= range.end_hz);
        // 1091 points (1877.18 Hz bins): 1.54 bins short of the span.
        assert_eq!(fe.fft_n(f64::from(g.rbw_hz)), Some(1091));
        assert!(g.covered.end_hz - g.covered.start_hz > 1_925_120 - 2 * 1878);
        // The nominal request radiod used to get (1880 Hz bins centred at
        // 7,100,940 Hz) really covers 6,136,180 .. 8,061,939 Hz.
        let old = ChannelStatus {
            radio_frequency_hz: Some(7_100_940.0),
            resolution_bw_hz: Some(1880.0),
            ..rtl
        };
        assert_eq!(
            covered_range(&old, 1024),
            Some(v1::FrequencyRange {
                start_hz: 6_136_180,
                end_hz: 8_061_939,
            })
        );
    }

    #[test]
    fn narrowband_geometry_tiles_the_range_exactly() {
        let range = v1::FrequencyRange {
            start_hz: 7_000_000,
            end_hz: 7_102_300,
        };
        for bins in [1023, 1024] {
            let g = geometry(&range, bins, Some(&siggen_fe())).unwrap();
            assert_eq!(g.covered, range);
            assert_eq!(covered_range(&spectrum_status(&g), bins), Some(range));
        }
        let g = geometry(&range, 1023, None).unwrap();
        assert_eq!(g.rbw_hz, 100.0);
        // Bin ⌈1023/2⌉ = 512 is at the centre.
        assert_eq!(g.center_hz, 7_000_000.0 + 512.5 * 100.0);
        assert!(covered_range(&ChannelStatus::default(), 64).is_none());
        let bad = ChannelStatus {
            resolution_bw_hz: Some(f64::NAN),
            ..spectrum_status(&g)
        };
        assert!(covered_range(&bad, 64).is_none());
        // Wideband needs the front end.
        let wide = ChannelStatus {
            resolution_bw_hz: Some(1000.0),
            first_lo_hz: None,
            ..spectrum_status(&g)
        };
        assert!(covered_range(&wide, 64).is_none());
    }

    #[test]
    fn quantization_defaults_and_checks() {
        assert_eq!(
            quantization(None).unwrap(),
            v1::Quantization {
                offset_db: -130.0,
                step_db: 0.5
            }
        );
        let q = v1::Quantization {
            offset_db: -100.0,
            step_db: 1.0,
        };
        assert_eq!(quantization(Some(q)).unwrap(), q);
        for bad in [
            v1::Quantization {
                offset_db: f32::NAN,
                step_db: 1.0,
            },
            v1::Quantization {
                offset_db: 0.0,
                step_db: 0.0,
            },
        ] {
            assert!(quantization(Some(bad)).is_err());
        }
    }

    #[test]
    fn signal_like_radiod_tune() {
        // radiod-notes.md §4's status: baseband -19.5 dB, N0 -91.5 dB/Hz,
        // ±5 kHz: 10·log10(10^-1.95 / (10^-9.15 · 10^4) - 1) ≈ 32 dB.
        let st = ChannelStatus {
            baseband_power_db: Some(-19.5),
            noise_density_db: Some(-91.5),
            low_edge_hz: Some(-5000.0),
            high_edge_hz: Some(5000.0),
            ..ChannelStatus::default()
        };
        let s = channel_signal(7, &st, None).unwrap();
        assert_eq!(s.channel_id, 7);
        assert!((s.snr_db - 32.0).abs() < 0.1, "{}", s.snr_db);
        assert_eq!(s.power_dbfs, -19.5);
        assert_eq!(s.squelch_open, None);
        assert_eq!(
            channel_signal(7, &st, Some(40.0)).unwrap().squelch_open,
            Some(false)
        );
        // FM reports its own SNR.
        let fm = ChannelStatus {
            fm_snr_db: Some(12.0),
            ..st.clone()
        };
        assert_eq!(channel_signal(1, &fm, Some(10.0)).unwrap().snr_db, 12.0);
        // Below the noise: clamped.
        let weak = ChannelStatus {
            baseband_power_db: Some(-200.0),
            ..st.clone()
        };
        assert_eq!(channel_signal(1, &weak, None).unwrap().snr_db, -100.0);
        for broken in [
            ChannelStatus {
                baseband_power_db: None,
                ..st.clone()
            },
            ChannelStatus {
                noise_density_db: Some(f64::NAN),
                ..st.clone()
            },
            ChannelStatus {
                high_edge_hz: Some(-5000.0),
                ..st.clone()
            },
            ChannelStatus {
                low_edge_hz: None,
                ..st
            },
        ] {
            assert!(channel_signal(1, &broken, None).is_none());
        }
    }

    proptest! {
        #[test]
        fn signal_values_are_finite_and_bounded(
            p in proptest::option::of(any::<f64>()),
            n in proptest::option::of(any::<f64>()),
            lo in proptest::option::of(any::<f64>()),
            hi in proptest::option::of(any::<f64>()),
            fm in proptest::option::of(any::<f64>()),
        ) {
            let st = ChannelStatus {
                baseband_power_db: p,
                noise_density_db: n,
                low_edge_hz: lo,
                high_edge_hz: hi,
                fm_snr_db: fm,
                ..ChannelStatus::default()
            };
            if let Some(s) = channel_signal(1, &st, Some(5.0)) {
                prop_assert!(s.snr_db.is_finite() && s.snr_db.abs() <= 100.0);
                prop_assert!(s.power_dbfs.is_finite());
            }
        }

        #[test]
        fn geometry_round_trips(start in 0u64..11_000_000, span in 13_000u64..12_000_000, bins in 64u32..8192) {
            let end = (start + span).min(12_000_000);
            prop_assume!(start < end);
            let range = v1::FrequencyRange { start_hz: start, end_hz: end };
            let g = geometry(&range, bins, Some(&siggen_fe())).unwrap();
            let covered = covered_range(&spectrum_status(&g), bins).unwrap();
            prop_assert_eq!(covered, g.covered);
            // Inside the request, short of it by less than two bins.
            let bin = (end - start) / u64::from(bins) + 1;
            prop_assert!(covered.start_hz >= start && covered.end_hz <= end);
            prop_assert!((covered.start_hz - start) + (end - covered.end_hz) <= 2 * bin);
        }

        #[test]
        fn tunable_range_is_ordered_when_present(
            rate in proptest::option::of(any::<u32>()),
            lo in proptest::option::of(any::<f64>()),
            fl in proptest::option::of(any::<f64>()),
            fh in proptest::option::of(any::<f64>()),
            real in proptest::option::of(any::<bool>()),
        ) {
            let st = ChannelStatus {
                input_samprate: rate,
                first_lo_hz: lo,
                fe_low_edge_hz: fl,
                fe_high_edge_hz: fh,
                fe_isreal: real,
                ..ChannelStatus::default()
            };
            if let Some(r) = tunable_range(&st) {
                prop_assert!(r.start_hz < r.end_hz);
                prop_assert!(r.end_hz <= limits::MAX_FREQUENCY_HZ);
            }
        }
    }
}
