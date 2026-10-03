// SPDX-License-Identifier: GPL-3.0-only
//! Measurement channels on radiod (station protocol 1.1, the measurement design §2.2), and
//! where radiod places a wideband spectrum.
//!
//! A measurement channel is a spectrum2 channel ([`PolledSpectrum`]) of
//! stationd's own, outside listener capacity, that stationd polls itself.
//! Per radiod-notes (§6, §9, §11, the measurement spike):
//! - `BIN_COUNT`, `RESOLUTION_BW` and `SPECTRUM_AVG` ([`SPECTRUM_AVG`]) are
//!   sent when the channel is created and never change: changing `BIN_COUNT`
//!   on a live channel crashes radiod, a change into narrowband mode (200 Hz
//!   bins or less) returns one wrong spectrum, and the averaging is part of
//!   the method (the low percentiles move with it);
//! - a hop sends `RADIO_FREQUENCY` only, on the same SSRC; its own reply
//!   carries a spectrum radiod computed from samples taken after the move, so
//!   nothing is discarded after a hop;
//! - one command at a time on the channel: [`PolledSpectrum`] has no
//!   background task, and stationd's polls (2/s) keep its lifetime fresh;
//! - closing sends `LIFETIME` 1 and waits for radiod's reply;
//! - the create command's reply has no bins; the first poll's is valid
//!   (within about 70 ms on an RTL-SDR, the measurement spike).
//!
//! **Placement.** radiod places a wideband spectrum on its analysis-FFT grid:
//! a window sits exactly where `RADIO_FREQUENCY` says only when its offset
//! from the front end's LO is a multiple of [`placement_grid_hz`] (1 kHz with
//! 500 Hz bins on an RTL-SDR or the signal generator); off it, up to one bin
//! closer to the LO ([`wideband_center`]). stationd puts its windows on that
//! grid (`Backend::measurement_grid`).

use station_backend::{BackendError, MeasurementChannel, MeasurementGrid, Spectrum};

use crate::client::{ClientError, Ka9qClient, PolledSpectrum, SpectrumParams};
use crate::spectrum::SpectrumFrame;
use crate::status::ChannelStatus;

/// FFTs averaged per spectrum (`SPECTRUM_AVG`), sent explicitly and never
/// changed: radiod's default today, and part of the measurement method (on
/// 40 m the 10th percentile is 0.8 dB lower at 10 than at 50, the measurement spike).
pub const SPECTRUM_AVG: u32 = 10;

/// Bins of this width or less put radiod's spectrum in narrowband mode, which
/// measurement channels never use.
pub const NARROWBAND_MAX_RBW_HZ: f32 = 200.0;

/// One open measurement channel.
#[derive(Debug)]
pub struct Measurement {
    channel: Option<PolledSpectrum>,
}

#[allow(clippy::cast_precision_loss, reason = "frequencies below 2^53 Hz")]
fn hz(v: u64) -> f64 {
    v as f64
}

fn rejected(e: ClientError) -> BackendError {
    match e {
        ClientError::InvalidParams(m) => BackendError::invalid_argument(m),
        other => BackendError::backend(other.to_string()),
    }
}

impl Measurement {
    /// Creates the spectrum2 channel (the create reply, without bins, is
    /// dropped).
    pub(super) async fn open(
        client: &Ka9qClient,
        ch: MeasurementChannel,
    ) -> Result<Self, BackendError> {
        if ch.resolution_bw_hz <= NARROWBAND_MAX_RBW_HZ {
            return Err(BackendError::invalid_argument(
                "measurement channels need bins wider than 200 Hz (radiod's wideband mode)",
            ));
        }
        let params = SpectrumParams {
            center_hz: hz(ch.center_hz),
            bin_count: ch.bin_count,
            resolution_bw_hz: ch.resolution_bw_hz,
            avg: Some(SPECTRUM_AVG),
            // Not used: the owner polls.
            row_interval: client.config().frame_duration,
        };
        let (channel, _created) = client
            .open_polled_spectrum(params)
            .await
            .map_err(rejected)?;
        tracing::debug!(
            ssrc = channel.ssrc(),
            center_hz = ch.center_hz,
            "measurement channel open"
        );
        Ok(Self {
            channel: Some(channel),
        })
    }

    fn channel(&mut self) -> Result<&mut PolledSpectrum, BackendError> {
        self.channel
            .as_mut()
            .ok_or_else(|| BackendError::not_found("measurement channel is closed"))
    }

    /// Moves the channel to `center_hz` (`RADIO_FREQUENCY` only); returns the
    /// spectrum of the reply.
    pub(super) async fn retune(
        &mut self,
        center_hz: u64,
    ) -> Result<Option<Spectrum>, BackendError> {
        let reply = self
            .channel()?
            .retune(hz(center_hz))
            .await
            .map_err(rejected)?;
        Ok(reply.frame.as_ref().map(spectrum))
    }

    /// Polls once.
    pub(super) async fn poll(&mut self) -> Result<Spectrum, BackendError> {
        let reply = self.channel()?.poll().await.map_err(rejected)?;
        reply.frame.as_ref().map(spectrum).ok_or_else(|| {
            BackendError::backend("radiod's reply carries no spectrum (the channel was re-created)")
        })
    }

    /// Closes the channel (`LIFETIME` 1).
    pub(super) async fn close(&mut self) -> Result<(), BackendError> {
        match self.channel.take() {
            Some(c) => c.close().await.map_err(rejected),
            None => Ok(()),
        }
    }
}

/// A row as per-bin power in dB (radiod's 8-bit log bins, decoded with the
/// row's own `SPECTRUM_BASE` and `SPECTRUM_STEP`). Bin `i` is centred at
/// `center_hz + (i − n/2) · bin_hz`, as [`SpectrumFrame::frequency_of_bin`].
pub fn spectrum(f: &SpectrumFrame) -> Spectrum {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a frequency in Hz, rounded"
    )]
    let center_hz = f.center_hz.max(0.0).round() as u64;
    #[allow(clippy::cast_possible_truncation, reason = "bin widths are small")]
    let bin_hz = f.bin_width_hz as f32;
    Spectrum {
        center_hz,
        bin_hz,
        power_db: f
            .bins
            .iter()
            .map(|&v| f.base_db + f32::from(v) * f.step_db)
            .collect(),
    }
}

/// The grid, in Hz from the front end's LO, on which radiod places a
/// wideband spectrum exactly: the least common multiple of the resolution
/// bandwidth and the front end's FFT bin (`samprate / (L + M − 1)`), when
/// both are whole numbers of Hz; otherwise `None`.
pub fn placement_grid_hz(
    rbw_hz: f64,
    samprate: u32,
    blocksize: u32,
    fir_length: u32,
) -> Option<f64> {
    let points = u64::from(blocksize) + u64::from(fir_length).checked_sub(1)?;
    if points == 0 || !rbw_hz.is_finite() || rbw_hz < 1.0 || rbw_hz.fract() != 0.0 {
        return None;
    }
    let samprate = u64::from(samprate);
    if samprate % points != 0 {
        return None;
    }
    let fe_bin = samprate / points;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "rbw is a positive whole number of Hz checked above"
    )]
    let rbw = rbw_hz as u64;
    let lcm = rbw / gcd(rbw, fe_bin) * fe_bin;
    #[allow(clippy::cast_precision_loss, reason = "grids are a few kHz")]
    Some(lcm as f64)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The centre radiod's wideband spectrum really has when asked for
/// `requested_hz` (`spectrum.c` `wideband_poll`, `radio.c` `compute_tuning`,
/// pinned commit): the offset from the LO is rounded to the front end's FFT
/// bin (`lrint`, ties to even), then scaled to the analysis FFT with C
/// integer division, which truncates toward zero. Off the placement grid the
/// spectrum is therefore up to one bin closer to the LO than
/// `RADIO_FREQUENCY` says.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "offsets are bounded by the sample rate, far below 2^52"
)]
pub fn wideband_center(
    requested_hz: f64,
    lo_hz: f64,
    samprate: u32,
    blocksize: u32,
    fir_length: u32,
    rbw_hz: f64,
) -> Option<f64> {
    let points = i64::from(blocksize) + i64::from(fir_length) - 1;
    if points <= 0 || samprate == 0 || rbw_hz.is_nan() || rbw_hz <= 0.0 {
        return None;
    }
    let samprate = f64::from(samprate);
    let fe_bin = samprate / points as f64;
    let fft_n = (samprate / rbw_hz).round_ties_even() as i64;
    let shift = ((requested_hz - lo_hz) / fe_bin).round_ties_even() as i64;
    let analysis_shift = shift * fft_n / points; // truncates toward zero, as C does
    Some(lo_hz + analysis_shift as f64 * samprate / fft_n as f64)
}

/// The measurement grid of the front end in `st` for `rbw_hz` bins: the LO
/// and [`placement_grid_hz`]; `None` when the status lacks a field or the
/// grid isn't whole.
pub fn grid_of(st: &ChannelStatus, rbw_hz: f32) -> Option<MeasurementGrid> {
    let lo = st.first_lo_hz?;
    let grid = placement_grid_hz(
        f64::from(rbw_hz),
        st.input_samprate?,
        st.filter_blocksize?,
        st.filter_fir_length?,
    )?;
    if !(lo.is_finite() && lo >= 0.0 && grid >= 1.0) {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "non-negative Hz, rounded"
    )]
    Some(MeasurementGrid {
        anchor_hz: lo.round() as u64,
        grid_hz: grid as u64,
    })
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact values of the 8-bit scale")]
mod tests {
    use super::*;

    #[test]
    fn rows_decode_to_power() {
        let f = SpectrumFrame {
            center_hz: 7_150_000.4,
            bin_width_hz: 500.0,
            base_db: -100.0,
            step_db: 0.5,
            bins: vec![0, 1, 200],
        };
        let s = spectrum(&f);
        assert_eq!(s.center_hz, 7_150_000);
        assert_eq!(s.bin_hz, 500.0);
        assert_eq!(s.power_db, [-100.0, -99.5, 0.0]);
    }

    #[test]
    fn placement_grid_of_the_rtlsdr_and_the_signal_generator() {
        // RTL-SDR: 2.048 MS/s, L = 40960, M = 10241 -> 40 Hz front-end bins.
        assert_eq!(
            placement_grid_hz(500.0, 2_048_000, 40_960, 10_241),
            Some(1000.0)
        );
        // sig_gen: 24 MS/s, L = 480000, M = 120001 -> 40 Hz.
        assert_eq!(
            placement_grid_hz(500.0, 24_000_000, 480_000, 120_001),
            Some(1000.0)
        );
        assert_eq!(placement_grid_hz(500.5, 2_048_000, 40_960, 10_241), None);
        assert_eq!(placement_grid_hz(500.0, 2_048_000, 40_960, 0), None);
        assert_eq!(placement_grid_hz(500.0, 2_000_000, 40_960, 10_241), None);
    }

    #[test]
    fn radiod_truncates_off_grid_centres_toward_the_lo() {
        let rtl = |f: f64| wideband_center(f, 7.1e6, 2_048_000, 40_960, 10_241, 500.0).unwrap();
        for f in [7.15e6, 7.1e6, 6.6e6, 7.612e6, 7.099e6] {
            assert!((rtl(f) - f).abs() < 1e-6, "{f}");
        }
        assert!((rtl(7_100_500.0) - 7_100_000.0).abs() < 1e-6);
        assert!((rtl(7_099_500.0) - 7_100_000.0).abs() < 1e-6);
        assert_eq!(wideband_center(1.0, 0.0, 0, 1, 1, 500.0), None);
    }

    #[test]
    fn the_grid_from_a_status() {
        let st = ChannelStatus {
            first_lo_hz: Some(7_100_000.0),
            input_samprate: Some(2_048_000),
            filter_blocksize: Some(40_960),
            filter_fir_length: Some(10_241),
            ..ChannelStatus::default()
        };
        assert_eq!(
            grid_of(&st, 500.0),
            Some(MeasurementGrid {
                anchor_hz: 7_100_000,
                grid_hz: 1000
            })
        );
        let partial = ChannelStatus {
            filter_fir_length: None,
            ..st
        };
        assert_eq!(grid_of(&partial, 500.0), None);
    }
}
