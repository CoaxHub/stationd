// SPDX-License-Identifier: GPL-3.0-only
//! The noise-floor reduction (the measurement design §2.2): spectra to the four quantiles
//! of a band's minute. Pure functions, no I/O: `stationd` runs them on every
//! backend's [`Spectrum`]s, and the measurement spike on radiod's rows
//! (`backend_ka9q::backend::measure::spectrum`).
//!
//! - Each bin is normalised to power spectral density, dB/Hz: power minus
//!   10·log10(bin width), as for the waterfall (station protocol §6.2), so a
//!   value doesn't move with the resolution.
//! - Every bin of every spectrum of a band in the minute goes into one
//!   [`Histogram`] of 0.1 dB steps from −250 to +50 dB/Hz (values outside
//!   are clamped to the ends).
//! - Only bins whose centre lies inside the band's accepted ranges are
//!   pooled; the ranges already exclude the edges of the tunable range and
//!   the DC guard (`stationd`'s window plan).
//! - The report carries the 10th, 25th, 50th and 90th percentiles (nearest
//!   rank, on the 0.1 dB grid, so p10 ≤ p25 ≤ p50 ≤ p90 always holds). The
//!   noise floor is p10; p90 − p10 is a rough activity figure.

use crate::Spectrum;

/// The lowest value a histogram holds, dB/Hz.
pub const MIN_DB_HZ: f32 = -250.0;
/// The highest value, dB/Hz.
pub const MAX_DB_HZ: f32 = 50.0;
/// Histogram cells per dB.
const CELLS_PER_DB: f32 = 10.0;
/// Cells from [`MIN_DB_HZ`] to [`MAX_DB_HZ`] inclusive.
const CELLS: usize = 3001;

/// Per-bin power (dB) to power spectral density (dB/Hz).
pub fn density_db_hz(power_db: f32, bin_hz: f32) -> f32 {
    power_db - 10.0 * bin_hz.max(f32::MIN_POSITIVE).log10()
}

/// The four quantiles of a band's minute, dB/Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quantiles {
    /// The noise floor.
    pub p10: f32,
    /// The 25th percentile.
    pub p25: f32,
    /// The median.
    pub p50: f32,
    /// The 90th percentile.
    pub p90: f32,
}

/// A 0.1 dB histogram of pooled densities, and what was pooled.
#[derive(Debug, Clone)]
pub struct Histogram {
    cells: Vec<u32>,
    count: u64,
    lowest_hz: u64,
    highest_hz: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            cells: vec![0; CELLS],
            count: 0,
            lowest_hz: u64::MAX,
            highest_hz: 0,
        }
    }
}

impl Histogram {
    /// Adds one density value.
    pub fn add(&mut self, db_hz: f32) {
        let v = if db_hz.is_nan() { MIN_DB_HZ } else { db_hz };
        let cell = ((v.clamp(MIN_DB_HZ, MAX_DB_HZ) - MIN_DB_HZ) * CELLS_PER_DB).round();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0..=3000"
        )]
        let cell = (cell as usize).min(CELLS - 1);
        if let Some(c) = self.cells.get_mut(cell) {
            *c = c.saturating_add(1);
        }
        self.count = self.count.saturating_add(1);
    }

    /// Adds every bin of `spectrum` whose centre lies inside `ranges`
    /// (`[start, end)`, sorted), normalised to density. Bin `i` is centred
    /// at `center_hz + (i − ⌈n/2⌉) · bin_hz` (radiod's layout). Returns how
    /// many.
    pub fn add_spectrum(&mut self, spectrum: &Spectrum, ranges: &[(u64, u64)]) -> u32 {
        let bin = f64::from(spectrum.bin_hz.max(f32::MIN_POSITIVE));
        #[allow(clippy::cast_precision_loss, reason = "lengths and Hz below 2^53")]
        let (half, centre) = (
            spectrum.power_db.len().div_ceil(2) as f64,
            spectrum.center_hz as f64,
        );
        let mut added = 0u32;
        for (i, &p) in spectrum.power_db.iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "i < 2^53")]
            let mid = centre + (i as f64 - half) * bin;
            let lo = mid - bin / 2.0;
            if mid < 0.0 {
                continue;
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "non-negative Hz below 2^53"
            )]
            let (mid_hz, lo_hz, hi_hz) = (mid as u64, lo.max(0.0) as u64, (lo + bin) as u64);
            if !ranges.iter().any(|(s, e)| *s <= mid_hz && mid_hz < *e) {
                continue;
            }
            self.add(density_db_hz(p, spectrum.bin_hz));
            self.lowest_hz = self.lowest_hz.min(lo_hz);
            self.highest_hz = self.highest_hz.max(hi_hz);
            added += 1;
        }
        added
    }

    /// Values pooled.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// `[lowest, highest]` frequency of the bins pooled, clipped to
    /// `bounds`; `None` when nothing was pooled.
    pub fn measured(&self, bounds: (u64, u64)) -> Option<(u64, u64)> {
        if self.count == 0 {
            return None;
        }
        let r = (self.lowest_hz.max(bounds.0), self.highest_hz.min(bounds.1));
        (r.0 < r.1).then_some(r)
    }

    /// The value of the cell holding the `rank`-th value (1-based).
    fn value_at_rank(&self, rank: u64) -> f32 {
        let mut seen = 0u64;
        for (i, &c) in self.cells.iter().enumerate() {
            seen += u64::from(c);
            if seen >= rank {
                #[allow(clippy::cast_precision_loss, reason = "i < 3001")]
                return MIN_DB_HZ + i as f32 / CELLS_PER_DB;
            }
        }
        MAX_DB_HZ
    }

    /// The `q` quantile (0 < q ≤ 1) by nearest rank.
    pub fn quantile(&self, q: f64) -> Option<f32> {
        if self.count == 0 {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "q in (0, 1], count < 2^53"
        )]
        let rank = ((q.clamp(0.0, 1.0) * self.count as f64).ceil() as u64).max(1);
        Some(round_tenth(self.value_at_rank(rank)))
    }

    /// p10, p25, p50 and p90; `None` when nothing was pooled.
    pub fn quantiles(&self) -> Option<Quantiles> {
        Some(Quantiles {
            p10: self.quantile(0.10)?,
            p25: self.quantile(0.25)?,
            p50: self.quantile(0.50)?,
            p90: self.quantile(0.90)?,
        })
    }
}

/// `v` on the 0.1 dB grid (removes float noise from the cell arithmetic).
fn round_tenth(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "quantiles are exact values on the 0.1 dB grid"
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn spectrum(center_hz: u64, bin_hz: f32, power_db: Vec<f32>) -> Spectrum {
        Spectrum {
            center_hz,
            bin_hz,
            power_db,
        }
    }

    #[test]
    fn density_does_not_move_with_the_bin_width() {
        // The same −81 dB/Hz floor at 1.8 kHz and at 6.25 Hz bins
        // (station protocol §6.2's 40 m measurement).
        assert!((density_db_hz(-81.0 + 10.0 * 1800f32.log10(), 1800.0) + 81.0).abs() < 1e-4);
        assert!((density_db_hz(-81.0 + 10.0 * 6.25f32.log10(), 6.25) + 81.0).abs() < 1e-4);
        assert!((density_db_hz(-100.0, 500.0) + 126.99).abs() < 0.01);
    }

    #[test]
    fn quantiles_by_nearest_rank() {
        let mut h = Histogram::default();
        assert_eq!(h.quantiles(), None);
        for i in 1..=100u8 {
            h.add(-130.0 + f32::from(i) / 10.0);
        }
        let q = h.quantiles().expect("values");
        assert_eq!(q.p10, -129.0);
        assert_eq!(q.p25, -127.5);
        assert_eq!(q.p50, -125.0);
        assert_eq!(q.p90, -121.0);
        assert_eq!(h.count(), 100);
        // A minority of strong signals barely moves the floor.
        for _ in 0..10 {
            h.add(-60.0);
        }
        assert_eq!(h.quantile(0.10), Some(-128.9));
        assert_eq!(h.quantile(0.90), Some(-120.1));
        assert_eq!(h.quantile(0.95), Some(-60.0));
    }

    #[test]
    fn values_are_clamped_to_the_grid() {
        let mut h = Histogram::default();
        h.add(-400.0);
        h.add(f32::NAN);
        h.add(80.0);
        h.add(f32::INFINITY);
        h.add(-123.456);
        let q = h.quantiles().expect("values");
        assert_eq!(q.p10, -250.0);
        assert_eq!(q.p50, -123.5);
        assert_eq!(q.p90, 50.0);
    }

    #[test]
    fn only_bins_inside_the_ranges_are_pooled() {
        // 8 bins of 1 kHz centred on 7.096 … 7.103 MHz.
        let s = spectrum(7_100_000, 1000.0, vec![-90.0; 8]);
        let mut h = Histogram::default();
        // The DC guard (7.098–7.102) takes the 4 middle bins out.
        let ranges = [(7_000_000, 7_098_000), (7_102_000, 7_300_000)];
        assert_eq!(h.add_spectrum(&s, &ranges), 4);
        assert_eq!(h.count(), 4);
        assert_eq!(
            h.measured((7_000_000, 7_300_000)),
            Some((7_095_500, 7_103_500))
        );
        assert_eq!(h.quantile(0.5), Some(-120.0));
        // Bins below 0 Hz are ignored, never wrapped.
        let low = spectrum(1_000, 1000.0, vec![-90.0; 8]);
        let mut h = Histogram::default();
        assert_eq!(h.add_spectrum(&low, &[(0, 10_000)]), 5);
        assert_eq!(Histogram::default().measured((0, 1)), None);
    }

    proptest! {
        #[test]
        fn quantiles_are_ordered_and_in_range(values in proptest::collection::vec(any::<f32>(), 1..500)) {
            let mut h = Histogram::default();
            for v in &values {
                h.add(*v);
            }
            let q = h.quantiles().expect("values");
            prop_assert!(q.p10 <= q.p25 && q.p25 <= q.p50 && q.p50 <= q.p90);
            for v in [q.p10, q.p25, q.p50, q.p90] {
                prop_assert!(v.is_finite() && (MIN_DB_HZ..=MAX_DB_HZ).contains(&v));
            }
        }
    }
}
