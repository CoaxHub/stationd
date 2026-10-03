// SPDX-License-Identifier: GPL-3.0-only
//! The noise-floor reduction of the measurement spike: window planning,
//! normalisation to dB/Hz, the pooled histogram and its quantiles, and the
//! UTC schedule. Pure functions only; `measure_run` drives radiod with them.
//!
//! - **Coverage.** The usable part of the front end's range: its tunable
//!   range minus a roll-off fraction at each end, minus a few kHz around the
//!   front end's centre (an RTL-SDR's DC spike).
//! - **Windows.** One wideband spectrum channel (fixed bin count and
//!   resolution) is moved between windows that tile the range to measure.
//!   radiod places a wideband spectrum on its analysis-FFT grid, so a
//!   window's centre is snapped to a grid on which radiod's placement is
//!   exact ([`placement_grid_hz`], [`wideband_center`]).
//! - **Statistic.** Every bin of every spectrum of a band in a UTC minute,
//!   normalised to dB/Hz as the waterfall is, goes into one histogram with
//!   0.1 dB steps; the minute's report is its 10th, 25th, 50th and 90th
//!   percentiles.

use backend_ka9q::spectrum::{SpectrumFrame, density_normalisation_db};

/// A frequency range `[lo_hz, hi_hz)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreqRange {
    /// Low edge, Hz.
    pub lo_hz: f64,
    /// High edge, Hz.
    pub hi_hz: f64,
}

impl FreqRange {
    /// A range, or `None` unless `lo_hz < hi_hz` and both are finite.
    pub fn new(lo_hz: f64, hi_hz: f64) -> Option<Self> {
        (lo_hz.is_finite() && hi_hz.is_finite() && lo_hz < hi_hz).then_some(Self { lo_hz, hi_hz })
    }

    /// Width, Hz.
    pub fn width(&self) -> f64 {
        self.hi_hz - self.lo_hz
    }

    /// The overlap of two ranges, if any.
    pub fn intersect(&self, other: &FreqRange) -> Option<FreqRange> {
        FreqRange::new(self.lo_hz.max(other.lo_hz), self.hi_hz.min(other.hi_hz))
    }

    /// Whether `[a, b)` overlaps this range.
    pub fn overlaps(&self, a: f64, b: f64) -> bool {
        a < self.hi_hz && b > self.lo_hz
    }
}

/// The usable part of a front end's tunable range `[fe_lo_hz, fe_hi_hz)`:
/// `rolloff` (a fraction, 0.05 for 5 %) of its width removed at each end
/// (the measurement design §2.1).
pub fn usable_range(fe_lo_hz: f64, fe_hi_hz: f64, rolloff: f64) -> Option<FreqRange> {
    let r = FreqRange::new(fe_lo_hz, fe_hi_hz)?;
    if !(0.0..0.5).contains(&rolloff) {
        return None;
    }
    let cut = r.width() * rolloff;
    FreqRange::new(r.lo_hz + cut, r.hi_hz - cut)
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

/// Centres of the windows of width `span_hz` that cover `range`, evenly
/// spread and snapped to multiples of `grid_hz` from `anchor_hz` (the front
/// end's LO). Windows are planned one grid step narrower than they are, so
/// that snapping never opens a gap. Empty for a degenerate span or grid.
pub fn plan_windows(range: FreqRange, span_hz: f64, grid_hz: f64, anchor_hz: f64) -> Vec<f64> {
    if !(span_hz > grid_hz && grid_hz > 0.0 && anchor_hz.is_finite()) {
        return Vec::new();
    }
    // Snapping moves a centre by up to half a grid step, so windows are planned
    // one grid step narrower than they are.
    let needed = (range.width() / (span_hz - grid_hz)).ceil().max(1.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a handful of windows"
    )]
    let n = needed as usize;
    #[allow(clippy::cast_precision_loss, reason = "a handful of windows")]
    (0..n)
        .map(|k| {
            let c = range.lo_hz + range.width() * (2 * k + 1) as f64 / (2 * n) as f64;
            anchor_hz + ((c - anchor_hz) / grid_hz).round() * grid_hz
        })
        .collect()
}

/// Power spectral density of bin value `v` of `frame`, dB/Hz: radiod's
/// per-bin power `base + v × step` minus `10·log10(bin width)`, as for the
/// waterfall (station protocol §6.2).
pub fn density_db_hz(frame: &SpectrumFrame, v: u8) -> f32 {
    frame.base_db + f32::from(v) * frame.step_db - density_normalisation_db(frame.bin_width_hz)
}

/// Every bin of `frame`, in dB/Hz.
pub fn densities(frame: &SpectrumFrame) -> Vec<f32> {
    frame
        .bins
        .iter()
        .map(|&v| density_db_hz(frame, v))
        .collect()
}

/// Indices of the bins of `frame` whose centre lies in `band` and which
/// overlap none of `exclude`.
pub fn band_bins(frame: &SpectrumFrame, band: &FreqRange, exclude: &[FreqRange]) -> Vec<usize> {
    let half = frame.bin_width_hz / 2.0;
    (0..frame.bins.len())
        .filter(|&i| {
            let f = frame.frequency_of_bin(i);
            f >= band.lo_hz
                && f < band.hi_hz
                && !exclude.iter().any(|x| x.overlaps(f - half, f + half))
        })
        .collect()
}

/// Histogram resolution, dB.
pub const HIST_STEP_DB: f32 = 0.1;
/// Lowest histogram value, dB/Hz; lower values count in the first bin.
pub const HIST_MIN_DB: f32 = -250.0;
/// Highest histogram value, dB/Hz; higher values count in the last bin.
pub const HIST_MAX_DB: f32 = 50.0;
/// Number of histogram bins.
pub const HIST_BINS: usize = 3000;

/// A histogram of densities in [`HIST_STEP_DB`] steps from [`HIST_MIN_DB`]
/// to [`HIST_MAX_DB`]. Out-of-range values are clamped into the end bins;
/// non-finite values are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    counts: Vec<u32>,
    total: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: vec![0; HIST_BINS],
            total: 0,
        }
    }
}

impl Histogram {
    /// Adds one value.
    pub fn add(&mut self, db: f32) {
        if !db.is_finite() {
            return;
        }
        let x = ((db - HIST_MIN_DB) / HIST_STEP_DB).floor();
        let last = (HIST_MAX_DB - HIST_MIN_DB) / HIST_STEP_DB - 1.0;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the bin range first"
        )]
        let i = (x.clamp(0.0, last) as usize).min(HIST_BINS - 1);
        self.counts[i] = self.counts[i].saturating_add(1);
        self.total += 1;
    }

    /// Adds the bins `idx` of `frame`, in dB/Hz. Returns how many.
    pub fn add_frame(&mut self, frame: &SpectrumFrame, idx: &[usize]) -> usize {
        let norm = density_normalisation_db(frame.bin_width_hz);
        let mut n = 0;
        for &i in idx {
            if let Some(&v) = frame.bins.get(i) {
                self.add(frame.base_db + f32::from(v) * frame.step_db - norm);
                n += 1;
            }
        }
        n
    }

    /// Values added.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The `q` quantile (0 < q ≤ 1) by nearest rank: the centre of the
    /// first bin whose cumulative count reaches `⌈q × total⌉`. `None` when
    /// empty or `q` is out of range.
    pub fn quantile(&self, q: f64) -> Option<f32> {
        if self.total == 0 || !(q > 0.0 && q <= 1.0) {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "totals stay far below 2^52"
        )]
        let rank = ((q * self.total as f64).ceil() as u64).max(1);
        let mut cum = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            cum += u64::from(c);
            if cum >= rank {
                #[allow(clippy::cast_precision_loss, reason = "i < 3000")]
                return Some(HIST_MIN_DB + (i as f32 + 0.5) * HIST_STEP_DB);
            }
        }
        None
    }

    /// The report's quantiles: p10, p25, p50, p90.
    pub fn quantiles(&self) -> Option<[f32; 4]> {
        Some([
            self.quantile(0.10)?,
            self.quantile(0.25)?,
            self.quantile(0.50)?,
            self.quantile(0.90)?,
        ])
    }
}

/// The `q` quantile of `values` by nearest rank (for one spectrum's
/// summary). `None` when empty or `q` is out of range.
pub fn quantile_of(values: &[f32], q: f64) -> Option<f32> {
    let mut v: Vec<f32> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() || !(q > 0.0 && q <= 1.0) {
        return None;
    }
    v.sort_by(f32::total_cmp);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a spectrum has at most 16k bins"
    )]
    let rank = ((q * v.len() as f64).ceil() as usize).max(1);
    v.get(rank - 1).copied()
}

/// Pearson correlation of two equally long series; `None` if they differ in
/// length or either is constant.
pub fn correlation(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.len() < 2 {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a spectrum has at most 16k bins"
    )]
    let n = a.len() as f64;
    let ma = a.iter().map(|&x| f64::from(x)).sum::<f64>() / n;
    let mb = b.iter().map(|&x| f64::from(x)).sum::<f64>() / n;
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        let (dx, dy) = (f64::from(x) - ma, f64::from(y) - mb);
        sab += dx * dy;
        saa += dx * dx;
        sbb += dy * dy;
    }
    let r = sab / (saa * sbb).sqrt();
    r.is_finite().then_some(r)
}

/// The index of the strongest bin.
pub fn peak_index(values: &[f32]) -> Option<usize> {
    values
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
}

/// Milliseconds in a minute.
pub const MINUTE_MS: i64 = 60_000;

/// The start of the UTC minute containing `unix_ms`.
pub fn minute_start(unix_ms: i64) -> i64 {
    unix_ms.div_euclid(MINUTE_MS) * MINUTE_MS
}

/// Whether the minute starting at `minute_ms` is a measuring minute of an
/// idle schedule with period `period_s` (a multiple of 60): its Unix time is
/// a multiple of the period (the measurement design §4.1).
pub fn is_measuring_minute(minute_ms: i64, period_s: i64) -> bool {
    period_s > 0 && minute_ms.rem_euclid(period_s * 1000) == 0
}

/// The first measuring minute whose lead time (`lead_ms` before it) has not
/// passed at `now_ms`.
pub fn next_measuring_minute(now_ms: i64, period_s: i64, lead_ms: i64) -> i64 {
    let period = (period_s * 1000).max(MINUTE_MS);
    let mut m = (now_ms + lead_ms).div_euclid(period) * period;
    if m - lead_ms < now_ms {
        m += period;
    }
    m
}

/// `unix_ms` as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[allow(clippy::many_single_char_names, reason = "date and time fields")]
pub fn utc_string(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (h, rest) = (ms_of_day / 3_600_000, ms_of_day % 3_600_000);
    let (mi, rest) = (rest / 60_000, rest % 60_000);
    let (s, ms) = (rest / 1000, rest % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact values by construction")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn frame(center_hz: f64, rbw: f64, bins: Vec<u8>) -> SpectrumFrame {
        SpectrumFrame {
            center_hz,
            bin_width_hz: rbw,
            base_db: -60.0,
            step_db: 0.5,
            bins,
        }
    }

    #[test]
    fn usable_range_cuts_the_rolloff() {
        // An RTL-SDR at 2.048 MS/s around 7.1 MHz.
        let r = usable_range(6_076_000.0, 8_124_000.0, 0.05).unwrap();
        assert!((r.lo_hz - 6_178_400.0).abs() < 1e-6);
        assert!((r.hi_hz - 8_021_600.0).abs() < 1e-6);
        assert!(usable_range(1.0, 0.0, 0.05).is_none());
        assert!(usable_range(0.0, 1.0, 0.5).is_none());
        assert!(usable_range(0.0, f64::NAN, 0.05).is_none());
    }

    #[test]
    fn ranges_intersect_and_overlap() {
        let a = FreqRange::new(7.0e6, 7.3e6).unwrap();
        let b = FreqRange::new(7.2e6, 8.0e6).unwrap();
        assert_eq!(a.intersect(&b), FreqRange::new(7.2e6, 7.3e6));
        assert_eq!(a.intersect(&FreqRange::new(8e6, 9e6).unwrap()), None);
        assert!(a.overlaps(6.9e6, 7.0e6 + 1.0));
        assert!(!a.overlaps(6.9e6, 7.0e6));
        assert!(FreqRange::new(1.0, 1.0).is_none());
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
        assert_eq!(
            placement_grid_hz(1000.0, 2_048_000, 40_960, 10_241),
            Some(1000.0)
        );
        assert_eq!(placement_grid_hz(500.5, 2_048_000, 40_960, 10_241), None);
        assert_eq!(placement_grid_hz(500.0, 2_048_000, 40_960, 0), None);
        assert_eq!(placement_grid_hz(500.0, 2_000_000, 40_960, 10_241), None);
    }

    #[test]
    fn radiod_truncates_off_grid_centres_toward_the_lo() {
        let rtl = |f: f64| wideband_center(f, 7.1e6, 2_048_000, 40_960, 10_241, 500.0).unwrap();
        // On the 1 kHz grid: exact, on both sides of the LO.
        for f in [7.15e6, 7.1e6, 6.6e6, 7.612e6, 7.099e6] {
            assert!((rtl(f) - f).abs() < 1e-6, "{f}");
        }
        // Half a grid step off: one 500 Hz bin closer to the LO.
        assert!((rtl(7_100_500.0) - 7_100_000.0).abs() < 1e-6);
        assert!((rtl(7_150_500.0) - 7_150_000.0).abs() < 1e-6);
        assert!((rtl(7_099_500.0) - 7_100_000.0).abs() < 1e-6);
        // A centre 0.5 bin off (the waterfall's tiling) is truncated too.
        assert!((rtl(7_100_250.0) - 7_100_000.0).abs() < 1e-6);
        // The signal generator (LO 0, 24 MS/s), as observed in the measurement spike: the
        // windows at 10.0005 and 10.1005 MHz sit 500 Hz lower; 9.9995 MHz
        // rounds away from zero and is exact.
        let sig = |f: f64| wideband_center(f, 0.0, 24_000_000, 480_000, 120_001, 500.0).unwrap();
        assert!((sig(10_000_500.0) - 10_000_000.0).abs() < 1e-6);
        assert!((sig(10_100_500.0) - 10_100_000.0).abs() < 1e-6);
        assert!((sig(9_999_500.0) - 9_999_500.0).abs() < 1e-6);
        assert!((sig(10_250_000.0) - 10_250_000.0).abs() < 1e-6);
        assert_eq!(wideband_center(1.0, 0.0, 0, 1, 1, 500.0), None);
    }

    #[test]
    fn windows_tile_the_range_on_the_grid() {
        let band = FreqRange::new(7.0e6, 7.3e6).unwrap();
        // 40 m fits in one 512 kHz window.
        assert_eq!(plan_windows(band, 512_000.0, 1000.0, 7.1e6), vec![7.15e6]);
        // The RTL-SDR's usable range takes 4.
        let r = FreqRange::new(6_178_400.0, 8_021_600.0).unwrap();
        let w = plan_windows(r, 512_000.0, 1000.0, 7.1e6);
        assert_eq!(w.len(), 4);
        for c in &w {
            assert_eq!(((c - 7.1e6) / 1000.0).fract(), 0.0, "{c} off the grid");
        }
        assert!(w[0] - 256_000.0 <= r.lo_hz && w[3] + 256_000.0 >= r.hi_hz);
        for pair in w.windows(2) {
            assert!(pair[1] - pair[0] <= 512_000.0, "gap between {pair:?}");
        }
        assert!(plan_windows(band, 1000.0, 1000.0, 7.1e6).is_empty());
        assert!(plan_windows(band, 512_000.0, 0.0, 7.1e6).is_empty());
    }

    proptest! {
        #[test]
        fn planned_windows_cover_any_range(
            lo in 1e6f64..30e6, width in 10e3f64..3e6, anchor in 1e6f64..30e6
        ) {
            let r = FreqRange::new(lo, lo + width).unwrap();
            let w = plan_windows(r, 512_000.0, 1000.0, anchor);
            prop_assert!(!w.is_empty());
            prop_assert!(w[0] - 256_000.0 <= r.lo_hz + 1e-6);
            prop_assert!(w[w.len() - 1] + 256_000.0 >= r.hi_hz - 1e-6);
            for pair in w.windows(2) {
                prop_assert!(pair[1] - pair[0] <= 512_000.0 + 1e-6);
            }
        }
    }

    #[test]
    fn density_subtracts_the_bin_width() {
        let f = frame(7.15e6, 500.0, vec![0, 20]);
        // -60 dB per 500 Hz bin = -60 - 26.99 dB/Hz.
        assert!((density_db_hz(&f, 0) - (-86.989_7)).abs() < 1e-3);
        assert!((density_db_hz(&f, 20) - (-76.989_7)).abs() < 1e-3);
        assert_eq!(densities(&f).len(), 2);
    }

    #[test]
    fn band_bins_respect_the_band_and_exclusions() {
        // 1024 bins of 500 Hz around 7.15 MHz: 6.894 .. 7.406 MHz.
        let f = frame(7.15e6, 500.0, vec![0; 1024]);
        let band = FreqRange::new(7.0e6, 7.3e6).unwrap();
        let dc = FreqRange::new(7.098e6, 7.102e6).unwrap();
        let all = band_bins(&f, &band, &[]);
        assert_eq!(all.len(), 600);
        assert_eq!(f.frequency_of_bin(all[0]), 7.0e6);
        assert_eq!(f.frequency_of_bin(*all.last().unwrap()), 7.2995e6);
        let no_dc = band_bins(&f, &band, &[dc]);
        // Bins centred 7.098 .. 7.102 MHz overlap ±2 kHz: 9 bins (the edge
        // bins at 7.0975 and 7.1025 MHz just touch it and stay).
        assert_eq!(all.len() - no_dc.len(), 9);
        assert!(
            no_dc
                .iter()
                .all(|&i| (f.frequency_of_bin(i) - 7.1e6).abs() > 2000.0)
        );
    }

    #[test]
    fn histogram_quantiles_by_nearest_rank() {
        let mut h = Histogram::default();
        assert_eq!(h.quantile(0.5), None);
        for i in 0..100u8 {
            // Bin centres, clear of the bin edges.
            h.add(-99.95 + f32::from(i) * 0.1);
        }
        h.add(f32::NAN);
        assert_eq!(h.total(), 100);
        let [p10, p25, p50, p90] = h.quantiles().unwrap();
        // Values -99.95, -99.85, ... -90.05; the 10th is -99.05.
        assert!((p10 - -99.05).abs() < 0.01, "{p10}");
        assert!((p25 - -97.55).abs() < 0.01, "{p25}");
        assert!((p50 - -95.05).abs() < 0.01, "{p50}");
        assert!((p90 - -91.05).abs() < 0.01, "{p90}");
        assert_eq!(h.quantile(0.0), None);
        assert_eq!(h.quantile(1.5), None);
        // Out-of-range values land in the end bins.
        let mut e = Histogram::default();
        e.add(-1000.0);
        e.add(1000.0);
        assert!((e.quantile(0.5).unwrap() - (HIST_MIN_DB + 0.05)).abs() < 1e-3);
        assert!((e.quantile(1.0).unwrap() - (HIST_MAX_DB - 0.05)).abs() < 1e-3);
    }

    #[test]
    fn p10_ignores_a_minority_of_signal_bins() {
        // 1024 noise bins at -80 dB/Hz ± 1 dB plus 20 % strong carriers.
        let mut h = Histogram::default();
        for i in 0..1024u16 {
            let noise = -80.0 + f32::from((i * 37) % 21) / 10.0 - 1.0;
            h.add(if i % 5 == 0 { -40.0 } else { noise });
        }
        let [p10, _, p50, p90] = h.quantiles().unwrap();
        assert!((-81.0..=-79.5).contains(&p10), "{p10}");
        assert!((-81.0..=-79.0).contains(&p50), "{p50}");
        assert!((p90 - -40.0).abs() < 0.1, "{p90}");
    }

    #[test]
    fn add_frame_normalises_like_the_waterfall() {
        let f = frame(7.15e6, 500.0, vec![10; 8]);
        let mut h = Histogram::default();
        assert_eq!(h.add_frame(&f, &[0, 1, 2, 99]), 3);
        let v = h.quantile(0.5).unwrap();
        assert!((v - density_db_hz(&f, 10)).abs() < 0.1, "{v}");
    }

    #[test]
    fn spectrum_helpers() {
        assert_eq!(quantile_of(&[3.0, 1.0, 2.0, f32::NAN], 0.5), Some(2.0));
        assert_eq!(quantile_of(&[], 0.5), None);
        assert_eq!(peak_index(&[1.0, 5.0, 2.0]), Some(1));
        assert_eq!(peak_index(&[]), None);
        let a = [1.0, 2.0, 3.0, 4.0];
        assert!((correlation(&a, &a).unwrap() - 1.0).abs() < 1e-12);
        assert!((correlation(&a, &[4.0, 3.0, 2.0, 1.0]).unwrap() + 1.0).abs() < 1e-12);
        assert_eq!(correlation(&a, &[1.0, 1.0, 1.0, 1.0]), None);
        assert_eq!(correlation(&a, &[1.0]), None);
    }

    #[test]
    fn the_idle_schedule_measures_every_fifth_minute() {
        // 2026-10-02T21:30:00Z is a measuring minute; 21:31 is not.
        let m = 1_790_976_600_000;
        assert_eq!(utc_string(m), "2026-10-02T21:30:00.000Z");
        assert!(is_measuring_minute(m, 300));
        assert!(!is_measuring_minute(m + MINUTE_MS, 300));
        assert!(is_measuring_minute(m + MINUTE_MS, 60));
        assert!(!is_measuring_minute(m, 0));
        assert_eq!(minute_start(m + 59_999), m);
        assert_eq!(minute_start(-1), -MINUTE_MS);
        // 5 s lead: at 21:29:54 the next one is 21:30; at 21:29:56 it is 21:35.
        assert_eq!(next_measuring_minute(m - 6000, 300, 5000), m);
        assert_eq!(next_measuring_minute(m - 5000, 300, 5000), m);
        assert_eq!(next_measuring_minute(m - 4000, 300, 5000), m + 300_000);
        assert_eq!(next_measuring_minute(m + 1, 300, 5000), m + 300_000);
    }

    /// The measurement spike's fixture: one row per measured minute, quantiles in order,
    /// every minute's samples the 40 m window's 591 bins times its spectra,
    /// and the idle rows only in measuring minutes.
    #[test]
    fn the_40m_fixture_is_well_formed() {
        let csv = include_str!("../../../testdata/noise-40m-sample.csv");
        let mut lines = csv.lines().filter(|l| !l.starts_with('#'));
        let header: Vec<&str> = lines.next().unwrap().split(',').collect();
        let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
        let (mut continuous, mut idle) = (0, 0);
        for line in lines {
            let f: Vec<&str> = line.split(',').collect();
            let num = |name: &str| f[col(name)].parse::<f64>().unwrap();
            let q = ["p10_db_hz", "p25_db_hz", "p50_db_hz", "p90_db_hz"].map(num);
            assert!(q.windows(2).all(|w| w[0] <= w[1]), "{line}");
            assert!((-100.0..-60.0).contains(&q[0]), "{line}");
            assert_eq!(num("samples"), num("spectra") * 591.0, "{line}");
            #[allow(clippy::cast_possible_truncation, reason = "Unix seconds")]
            let minute_ms = num("minute_unix_s") as i64 * 1000;
            assert_eq!(utc_string(minute_ms), f[col("minute_utc")]);
            match f[col("duty")] {
                "continuous" => continuous += 1,
                "idle" => {
                    assert!(is_measuring_minute(minute_ms, 300), "{line}");
                    idle += 1;
                }
                other => panic!("duty {other}"),
            }
        }
        assert_eq!((continuous, idle), (60, 40));
    }

    #[test]
    fn utc_strings() {
        assert_eq!(utc_string(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_string(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(utc_string(-1), "1969-12-31T23:59:59.999Z");
    }
}
