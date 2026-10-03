// SPDX-License-Identifier: GPL-3.0-only
//! Where radiod really puts a spectrum2 channel's bins (radiod-notes.md §6,
//! §10 "Waterfall geometry", §11), at the pinned commit. Pure functions only.
//!
//! - **Layout.** Bin `i` of `N` (low to high) is the signed offset
//!   `i − ⌈N/2⌉` from the spectrum's centre: with an odd count there is one
//!   more bin below the centre than above (`encode_byte_data` starts at
//!   `bin_count/2` of a DC-first array whose upper `N − N/2` entries are the
//!   negative offsets). Both modes.
//! - **Narrowband** (`RESOLUTION_BW` ≤ [`CROSSOVER_HZ`]): radiod downconverts
//!   with fine tuning, so the centre is `RADIO_FREQUENCY` and the bin width
//!   `RESOLUTION_BW`, exactly.
//! - **Wideband** (above it): the bins are FFT bins of the A/D stream.
//!   `fft_n = lrint(samprate / RESOLUTION_BW)` (`setup_wideband`), so the bin
//!   width is `samprate / fft_n`. The centre's offset from the LO is rounded
//!   to the front end's FFT bin, `samprate / (L + M − 1)` (`compute_tuning`,
//!   `lrint`), then scaled to the analysis FFT with C integer division, which
//!   truncates toward zero (`wideband_poll`: `bin_shift * fft_n / points`).
//!   So the centre is `LO + k × samprate / fft_n` for an integer `k`, up to
//!   one bin closer to the LO than `RADIO_FREQUENCY` says.
//! - **Real front ends** in wideband mode lose the whole lower half of the
//!   window when its upper half runs past Nyquist (`wideband_poll` stops its
//!   loop at bin `fft_n/2`): it needs `k + ⌊N/2⌋ ≤ ⌊fft_n/2⌋`, and nothing
//!   below 0 Hz.
//!
//! [`plan_wideband`] picks the request whose real bins tile a range: a bin
//! width radiod can produce, an analysis shift whose bins lie inside the
//! range, and a `RADIO_FREQUENCY` on the front end's grid that radiod maps to
//! exactly that shift. [`placement`] says where a request really lands.

use crate::status::ChannelStatus;

/// radiod's default `CROSSOVER`: bins of this width or less are narrowband.
pub const CROSSOVER_HZ: f64 = 200.0;

/// The front end, as far as spectrum placement goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrontEnd {
    /// `FIRST_LO_FREQUENCY`, Hz.
    pub lo_hz: f64,
    /// `INPUT_SAMPRATE`, Hz.
    pub samprate: u32,
    /// Points of the front end's FFT, `L + M − 1` (`FILTER_BLOCKSIZE`,
    /// `FILTER_FIR_LENGTH`).
    pub points: u32,
    /// `FE_ISREAL`.
    pub real: bool,
}

impl FrontEnd {
    /// The front end of a radiod status, if it has every field.
    pub fn from_status(st: &ChannelStatus) -> Option<Self> {
        let lo_hz = st.first_lo_hz.filter(|f| f.is_finite())?;
        let samprate = st.input_samprate.filter(|r| *r > 0)?;
        let points = st
            .filter_blocksize?
            .checked_add(st.filter_fir_length?.checked_sub(1)?)?;
        (points > 0).then_some(Self {
            lo_hz,
            samprate,
            points,
            real: st.fe_isreal.unwrap_or(false),
        })
    }

    fn rate(&self) -> f64 {
        f64::from(self.samprate)
    }

    /// The front end's FFT bin, Hz.
    pub fn fe_bin_hz(&self) -> f64 {
        self.rate() / f64::from(self.points)
    }

    /// radiod's analysis FFT length for `rbw_hz` bins in wideband mode,
    /// `lrint(samprate / rbw)`.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded by the sample rate first"
    )]
    pub fn fft_n(&self, rbw_hz: f64) -> Option<i64> {
        let n = (self.rate() / rbw_hz).round_ties_even();
        (n.is_finite() && n >= 1.0 && n <= self.rate()).then_some(n as i64)
    }

    /// The front-end bin shift radiod computes for a channel at `freq_hz`
    /// (`compute_tuning`: `lrint`, ties to even), or `None` when it is out
    /// of the front end's coverage (radiod then idles the channel).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded by the front end's points first"
    )]
    pub fn fe_shift(&self, freq_hz: f64) -> Option<i64> {
        let s = ((freq_hz - self.lo_hz) / self.fe_bin_hz()).round_ties_even();
        let half = f64::from(self.points / 2);
        (s.is_finite() && s.abs() < half).then_some(s as i64)
    }

    /// The analysis-FFT shift of a front-end shift: C integer division,
    /// which truncates toward zero (Rust's `/` on integers does the same).
    pub fn analysis_shift(&self, fe_shift: i64, fft_n: i64) -> i64 {
        fe_shift * fft_n / i64::from(self.points)
    }

    /// The `RADIO_FREQUENCY` on the front end's grid that radiod maps to
    /// analysis shift `k` with `fft_n` points, if there is one.
    fn request_for(&self, k: i64, fft_n: i64) -> Option<f64> {
        #[allow(clippy::cast_precision_loss, reason = "shifts are below 2^31")]
        let s0 = (k as f64) * f64::from(self.points) / (fft_n as f64);
        #[allow(clippy::cast_possible_truncation, reason = "checked to be finite")]
        let around = s0.is_finite().then(|| s0.floor() as i64)?;
        // Away from zero first: truncation then lands on k when fft_n < points.
        let order = if k >= 0 { [1, 0, 2, -1] } else { [0, 1, -1, 2] };
        order.into_iter().map(|d| around + d).find_map(|s| {
            #[allow(clippy::cast_precision_loss, reason = "shifts are below 2^31")]
            let f = self.lo_hz + s as f64 * self.fe_bin_hz();
            (self.fe_shift(f) == Some(s) && self.analysis_shift(s, fft_n) == k).then_some(f)
        })
    }
}

/// The index of the bin at a spectrum's centre, `⌈bins/2⌉`.
pub fn center_index(bins: u32) -> u32 {
    bins.div_ceil(2)
}

/// Where a spectrum's bins are: bin `i` is centred on
/// `center_hz + (i − ⌈bins/2⌉) × bin_hz`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// The frequency of bin `⌈bins/2⌉`, Hz.
    pub center_hz: f64,
    /// Bin spacing, Hz.
    pub bin_hz: f64,
    /// Bin count.
    pub bins: u32,
}

impl Placement {
    /// The centre of bin `i`.
    pub fn bin_center(&self, i: u32) -> f64 {
        self.center_hz + (f64::from(i) - f64::from(center_index(self.bins))) * self.bin_hz
    }

    /// The low edge of bin 0.
    pub fn low_edge(&self) -> f64 {
        self.bin_center(0) - self.bin_hz / 2.0
    }

    /// The high edge of the last bin.
    pub fn high_edge(&self) -> f64 {
        self.low_edge() + f64::from(self.bins) * self.bin_hz
    }
}

/// Where radiod puts a spectrum asked for at `rf_hz` with `rbw_hz` bins:
/// exactly there in narrowband mode, on the analysis FFT's grid in wideband
/// mode. `None` for unusable values, a wideband request without `fe`, or a
/// centre outside the front end's coverage.
#[allow(
    clippy::cast_precision_loss,
    reason = "shifts and FFT sizes are below 2^31"
)]
pub fn placement(fe: Option<&FrontEnd>, rf_hz: f64, rbw_hz: f64, bins: u32) -> Option<Placement> {
    if !(rf_hz.is_finite() && rbw_hz.is_finite() && rbw_hz > 0.0 && bins > 0) {
        return None;
    }
    if rbw_hz <= CROSSOVER_HZ {
        return Some(Placement {
            center_hz: rf_hz,
            bin_hz: rbw_hz,
            bins,
        });
    }
    let fe = fe?;
    let fft_n = fe.fft_n(rbw_hz)?;
    let k = fe.analysis_shift(fe.fe_shift(rf_hz)?, fft_n);
    let bin_hz = fe.rate() / fft_n as f64;
    Some(Placement {
        center_hz: fe.lo_hz + k as f64 * bin_hz,
        bin_hz,
        bins,
    })
}

/// A wideband spectrum request and where its bins land.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidebandRequest {
    /// `RADIO_FREQUENCY` to send.
    pub center_hz: f64,
    /// `RESOLUTION_BW` to send: the real bin width, as radiod's float.
    pub rbw_hz: f32,
    /// Where the bins are.
    pub placement: Placement,
}

/// Most bin widths [`plan_wideband`] tries.
const MAX_WIDTHS: i64 = 100_000;
/// Shifts within one part in 10^9 of a bin of an edge still fit.
const EPS: f64 = 1e-9;

/// The wideband request whose `bins` real bins lie inside `[start_hz,
/// end_hz]`, as wide as possible and centred in it: the widest bin
/// `samprate / fft_n` (at most `(end − start) / bins`) for which some
/// analysis shift fits, the shift nearest the centre, and a
/// `RADIO_FREQUENCY` radiod maps to that shift. `None` when the bins would
/// be narrowband or nothing fits.
///
/// The bins fall short of the range by less than two bins when it is no
/// wider than the sample rate: up to one for the whole shift, and up to
/// `(end − start) / samprate` of a bin per step of `fft_n`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "FFT sizes and shifts are below 2^31, checked as f64 first"
)]
pub fn plan_wideband(
    fe: &FrontEnd,
    start_hz: f64,
    end_hz: f64,
    bins: u32,
) -> Option<WidebandRequest> {
    let span = end_hz - start_hz;
    if !(span.is_finite() && span > 0.0 && bins > 0) {
        return None;
    }
    let n = i64::from(bins);
    let c = i64::from(center_index(bins));
    let first = (fe.rate() * n as f64 / span).ceil();
    if !(first.is_finite() && first >= 1.0 && first <= fe.rate()) {
        return None;
    }
    for fft_n in (first as i64)..(first as i64).saturating_add(MAX_WIDTHS) {
        let bin_hz = fe.rate() / fft_n as f64;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "RESOLUTION_BW is a C float"
        )]
        let rbw_hz = bin_hz as f32;
        if f64::from(rbw_hz) <= CROSSOVER_HZ {
            return None;
        }
        if fe.fft_n(f64::from(rbw_hz)) != Some(fft_n) {
            continue;
        }
        // Bin j is centred on LO + j × bin_hz; the window's bins are
        // j = k − c ..= k − c + n − 1.
        let lo = ((start_hz - fe.lo_hz) / bin_hz + c as f64 + 0.5 - EPS).ceil();
        let hi = ((end_hz - fe.lo_hz) / bin_hz + c as f64 + 0.5 - n as f64 + EPS).floor();
        if !(lo.abs() < 1e12 && hi.abs() < 1e12) {
            return None;
        }
        let (mut lo, mut hi) = (lo as i64, hi as i64);
        // What radiod's loops can fill (`wideband_poll`).
        if fe.real {
            lo = lo.max(c);
            hi = hi.min(fft_n / 2 - n / 2);
        } else {
            lo = lo.max(c - fft_n / 2);
            hi = hi.min((fft_n + 1) / 2 - n + c);
        }
        if lo > hi {
            continue;
        }
        let mid = f64::midpoint(start_hz, end_hz);
        let target = ((mid - fe.lo_hz) / bin_hz + c as f64 - (n - 1) as f64 / 2.0).round() as i64;
        let target = target.clamp(lo, hi);
        let nearest = (0..8)
            .flat_map(|d| [target - d, target + d])
            .filter(|k| (lo..=hi).contains(k));
        for k in nearest {
            if let Some(center_hz) = fe.request_for(k, fft_n) {
                return Some(WidebandRequest {
                    center_hz,
                    rbw_hz,
                    placement: Placement {
                        center_hz: fe.lo_hz + k as f64 * bin_hz,
                        bin_hz,
                        bins,
                    },
                });
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact values by construction")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// An RTL-SDR at 2.048 MS/s (`L` 40,960, `M` 10,241), LO 7.1 MHz.
    const RTL: FrontEnd = FrontEnd {
        lo_hz: 7.1e6,
        samprate: 2_048_000,
        points: 51_200,
        real: false,
    };
    /// The test signal generator: real, 24 MS/s, LO 0 (`L` 480,000, `M` 120,001).
    const SIG_GEN: FrontEnd = FrontEnd {
        lo_hz: 0.0,
        samprate: 24_000_000,
        points: 600_000,
        real: true,
    };
    /// The complex signal generator of the placement checks: 2.048 MS/s, LO
    /// 1.024 MHz, carrier at 100 kHz.
    const SIG_GEN_IQ: FrontEnd = FrontEnd {
        lo_hz: 1_024_000.0,
        samprate: 2_048_000,
        points: 51_200,
        real: false,
    };
    /// An RX888 at 64.8 MS/s, real, LO 0 (`L` 1,296,000, `M` 324,001).
    const RX888: FrontEnd = FrontEnd {
        lo_hz: 0.0,
        samprate: 64_800_000,
        points: 1_620_000,
        real: true,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// The bin a carrier at `f` falls in.
    #[allow(clippy::cast_possible_truncation, reason = "test values")]
    fn bin_of(p: &Placement, f: f64) -> i64 {
        ((f - p.center_hz) / p.bin_hz).round() as i64 + i64::from(center_index(p.bins))
    }

    #[test]
    fn the_front_end_from_a_status() {
        let st = ChannelStatus {
            first_lo_hz: Some(7.1e6),
            input_samprate: Some(2_048_000),
            filter_blocksize: Some(40_960),
            filter_fir_length: Some(10_241),
            fe_isreal: Some(false),
            ..ChannelStatus::default()
        };
        assert_eq!(FrontEnd::from_status(&st), Some(RTL));
        assert_eq!(RTL.fe_bin_hz(), 40.0);
        for broken in [
            ChannelStatus {
                filter_fir_length: Some(0),
                ..st.clone()
            },
            ChannelStatus {
                first_lo_hz: Some(f64::NAN),
                ..st.clone()
            },
            ChannelStatus {
                input_samprate: Some(0),
                ..st
            },
        ] {
            assert_eq!(FrontEnd::from_status(&broken), None);
        }
    }

    #[test]
    fn odd_counts_have_one_more_bin_below_the_centre() {
        assert_eq!((center_index(1024), center_index(1023)), (512, 512));
        assert_eq!(center_index(1025), 513);
        let p = Placement {
            center_hz: 1000.0,
            bin_hz: 10.0,
            bins: 5,
        };
        assert_eq!(p.bin_center(0), 970.0);
        assert_eq!(p.bin_center(4), 1010.0);
        assert_eq!((p.low_edge(), p.high_edge()), (965.0, 1015.0));
    }

    /// Gate 0a: 500 Hz bins on the signal generator and the RTL-SDR.
    #[test]
    fn radiod_truncates_off_grid_centres_toward_the_lo() {
        let rtl = |f: f64| placement(Some(&RTL), f, 500.0, 1024).unwrap().center_hz;
        for f in [7.15e6, 7.1e6, 6.6e6, 7.612e6, 7.099e6] {
            assert!(close(rtl(f), f), "{f}");
        }
        assert!(close(rtl(7_100_500.0), 7_100_000.0));
        assert!(close(rtl(7_099_500.0), 7_100_000.0));
        assert!(close(rtl(7_100_250.0), 7_100_000.0));
        let sig = |f: f64| placement(Some(&SIG_GEN), f, 500.0, 1024).unwrap().center_hz;
        assert!(close(sig(10_000_500.0), 10_000_000.0));
        assert!(close(sig(10_100_500.0), 10_100_000.0));
        assert!(close(sig(9_999_500.0), 9_999_500.0));
        assert!(close(sig(10_250_000.0), 10_250_000.0));
    }

    /// The RTL-SDR's full span, 6,137,440 .. 8,062,560 Hz in 1024 bins, as
    /// the nominal geometry asked for it: 1880 Hz bins centred at
    /// 7,100,940 Hz. radiod runs 1089 points (1880.62 Hz bins) centred at
    /// the LO.
    #[test]
    fn the_nominal_full_span_request_is_off_by_up_to_a_bin() {
        let p = placement(Some(&RTL), 7_100_940.0, 1880.0, 1024).unwrap();
        assert_eq!(RTL.fft_n(1880.0), Some(1089));
        assert!(close(p.center_hz, 7.1e6));
        assert!(close(p.bin_hz, 2_048_000.0 / 1089.0));
        let nominal = |i: u32| 7_100_940.0 + (f64::from(i) - 512.0) * 1880.0;
        let errors: Vec<f64> = [0, 512, 1023]
            .map(|i| nominal(i) - p.bin_center(i))
            .to_vec();
        assert!((errors[0] - 1259.3).abs() < 1.0, "{errors:?}");
        assert!((errors[1] - 940.0).abs() < 1e-6, "{errors:?}");
        assert!((errors[2] - 621.3).abs() < 1.0, "{errors:?}");
    }

    /// Observed on the complex signal generator (carrier 100 kHz): radiod's
    /// peak bin for these requests (radiod-notes.md §10).
    #[test]
    fn the_carrier_lands_where_the_model_says() {
        let observed: [(f64, f64, u32, i64); 15] = [
            (1_024_940.0, 1880.0, 1024, 21),
            (1_024_000.0, 1880.0, 1024, 21),
            (1_024_000.0, 2000.0, 1024, 50),
            (1_024_000.0, 2000.0, 1023, 50),
            (1_024_000.0, 2000.0, 1025, 51),
            (1_024_000.0, 1880.0, 1023, 21),
            (1_018_000.0, 1880.0, 1024, 24),
            (1_019_557.0, 1880.0, 1024, 23),
            (1_027_342.0, 1880.0, 1024, 20),
            (1_028_899.0, 1880.0, 1024, 19),
            (1_018_422.0, 2730.0, 701, 15),
            (1_020_321.0, 2730.0, 701, 14),
            (1_026_018.0, 2730.0, 701, 13),
            (1_027_917.0, 2730.0, 701, 12),
            (1_029_816.0, 2730.0, 701, 11),
        ];
        for (rf, rbw, bins, peak) in observed {
            let p = placement(Some(&SIG_GEN_IQ), rf, rbw, bins).unwrap();
            assert_eq!(bin_of(&p, 100_000.0), peak, "{rf} {rbw} {bins}");
        }
        // Narrowband: exactly as asked, odd counts included.
        for bins in [1023, 1024] {
            let p = placement(Some(&SIG_GEN_IQ), 102_000.0, 100.0, bins).unwrap();
            assert_eq!(bin_of(&p, 100_000.0), 492);
        }
        assert_eq!(placement(None, 7e6, 100.0, 64).unwrap().center_hz, 7e6);
        assert_eq!(placement(None, 7e6, 500.0, 64), None);
    }

    #[test]
    fn the_full_span_plan_tiles_inside_the_range() {
        let w = plan_wideband(&RTL, 6_137_440.0, 8_062_560.0, 1024).unwrap();
        let p = w.placement;
        assert_eq!(
            placement(Some(&RTL), w.center_hz, f64::from(w.rbw_hz), 1024),
            Some(p)
        );
        assert!(p.low_edge() >= 6_137_440.0 && p.high_edge() <= 8_062_560.0);
        // Less than two bins short of the request.
        assert!(p.high_edge() - p.low_edge() > 8_062_560.0 - 6_137_440.0 - 2.0 * p.bin_hz);
        // The signal generator's 0 .. 12 MHz in 256 bins: a real front end,
        // so the window stays above 0 Hz and its top half below Nyquist.
        let w = plan_wideband(&SIG_GEN, 0.0, 12e6, 256).unwrap();
        let fft_n = SIG_GEN.fft_n(f64::from(w.rbw_hz)).unwrap();
        let k = SIG_GEN.analysis_shift(SIG_GEN.fe_shift(w.center_hz).unwrap(), fft_n);
        assert!(k + 128 <= fft_n / 2, "k {k} fft_n {fft_n}");
        assert!(w.placement.low_edge() > 0.0 && w.placement.high_edge() <= 12e6);
        // Narrowband widths are not planned here.
        assert_eq!(plan_wideband(&RTL, 7.0e6, 7.1e6, 1024), None);
    }

    /// Observed on the real signal generator (24 MS/s, `fft_n` 12,000 at
    /// 2 kHz bins): the 10 MHz carrier shows at k 5488 and is gone at 5489,
    /// whose lower half radiod leaves empty.
    #[test]
    fn real_front_ends_lose_the_lower_half_past_nyquist() {
        let k = |rf: f64| {
            SIG_GEN.analysis_shift(
                SIG_GEN.fe_shift(rf).unwrap(),
                SIG_GEN.fft_n(2000.0).unwrap(),
            )
        };
        assert_eq!((k(10_976_000.0), k(10_978_000.0)), (5488, 5489));
        // A plan near Nyquist stays clear of it.
        let w = plan_wideband(&SIG_GEN, 10_000_000.0, 12_000_000.0, 1024).unwrap();
        let fft_n = SIG_GEN.fft_n(f64::from(w.rbw_hz)).unwrap();
        assert!(
            SIG_GEN.analysis_shift(SIG_GEN.fe_shift(w.center_hz).unwrap(), fft_n) + 512
                <= fft_n / 2
        );
    }

    fn front_end() -> impl Strategy<Value = FrontEnd> {
        prop_oneof![
            (6.0e6f64..30e6).prop_map(|lo| FrontEnd { lo_hz: lo, ..RTL }),
            Just(SIG_GEN),
            Just(SIG_GEN_IQ),
            Just(RX888),
        ]
    }

    proptest! {
        /// Any range inside the front end's coverage, any wideband bin
        /// width: the plan's bins lie inside the range, cover all but less
        /// than two bins of it, and radiod puts them where the plan says.
        #[test]
        fn plans_land_where_they_say(
            fe in front_end(),
            a in 0.02f64..0.98,
            b in 0.02f64..0.98,
            bins in 64u32..8192,
        ) {
            let rate = f64::from(fe.samprate);
            let (cov_lo, cov_hi) = if fe.real { (0.0, rate / 2.0) } else { (fe.lo_hz - rate / 2.0, fe.lo_hz + rate / 2.0) };
            let (a, b) = (a.min(b), a.max(b));
            let start = (cov_lo + a * (cov_hi - cov_lo)).round();
            let end = (cov_lo + b * (cov_hi - cov_lo)).round();
            prop_assume!((end - start) / f64::from(bins) > 2.0 * CROSSOVER_HZ);
            let w = plan_wideband(&fe, start, end, bins).unwrap();
            let p = w.placement;
            prop_assert_eq!(placement(Some(&fe), w.center_hz, f64::from(w.rbw_hz), bins), Some(p));
            prop_assert!(p.low_edge() >= start - 1e-6 && p.high_edge() <= end + 1e-6);
            prop_assert!(p.high_edge() - p.low_edge() > (end - start) - 2.0 * p.bin_hz);
            prop_assert!(p.bin_hz > CROSSOVER_HZ && p.bin_hz <= (end - start) / f64::from(bins));
            let fft_n = fe.fft_n(f64::from(w.rbw_hz)).unwrap();
            let k = fe.analysis_shift(fe.fe_shift(w.center_hz).unwrap(), fft_n);
            let c = i64::from(center_index(bins));
            if fe.real {
                prop_assert!(k - c >= 0 && k + i64::from(bins / 2) <= fft_n / 2);
            }
        }
    }
}
