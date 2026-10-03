// SPDX-License-Identifier: GPL-3.0-only
//! Window planning (the measurement design §2.2, station protocol §4.1): which windows the
//! measurement channel visits, and in which measured minute.
//!
//! - A **window** is one position of the measurement channel: `bin_count ×
//!   resolution_bw_hz` wide (1024 × 500 Hz = 512 kHz), centred inside the
//!   tunable range so the channel never asks for a frequency the front end
//!   doesn't cover. A band is covered by the fewest windows that span its
//!   accepted ranges; a band wider than one minute of windows is clipped to
//!   the first minute's worth (its accepted range shrinks).
//! - A minute holds `60 s / dwell` **slots** (15 with a 4 s dwell). Bands go
//!   into minutes in order, whole: a band never spans two minutes. One cycle
//!   visits every window once; it takes 1, 2 or 5 measured minutes, and bands
//!   that don't fit 5 are not accepted.
//! - Inside a minute the slots go round-robin over the minute's windows, so a
//!   single window is polled all minute long.
//!
//! **Placement.** When the backend has a placement grid
//! (`Backend::measurement_grid`: radiod places a wideband spectrum exactly
//! only when its offset from the LO is a multiple of 1 kHz with 500 Hz bins),
//! every window centre is snapped to it, and windows are planned one grid
//! step narrower than they are so snapping never opens a gap.
//!
//! Before planning, each band's ranges are cut to the usable tunable range
//! (5 % off each end) minus ±2 kHz around the front-end centre: the same
//! coverage rules the hub plans with, applied again on the station.

use station_backend::MeasurementGrid;

/// The share of the tunable range left out at each end, percent (the
/// anti-alias roll-off).
pub const EDGE_PERCENT: u64 = 5;

/// Half the width left out around the front-end centre, Hz (an RTL-SDR's DC
/// spike).
pub const DC_GUARD_HZ: u64 = 2_000;

/// Measured minutes a cycle may stretch over.
pub const CYCLE_MINUTES: [u32; 3] = [1, 2, 5];

/// A requested band: id, key, ranges `[start, end)`.
pub type BandRequest = (u32, String, Vec<(u64, u64)>);

/// One band of a plan, as accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Hub-assigned.
    pub band_id: u32,
    /// For logs only.
    pub key: String,
    /// The accepted ranges `[start, end)`, sorted and apart.
    pub ranges: Vec<(u64, u64)>,
}

/// One position of the measurement channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Where the channel is tuned, Hz.
    pub center_hz: u64,
    /// Indices into [`Plan::bands`] of the bands this window pools for.
    pub bands: Vec<usize>,
}

/// A planned measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The accepted bands, in the requested order.
    pub bands: Vec<Band>,
    /// The windows of each measured minute of a cycle (`minutes.len()` is
    /// 1, 2 or 5; a minute may be empty when the cycle is rounded up).
    pub minutes: Vec<Vec<Window>>,
    /// Slots per minute.
    pub slots_per_minute: u32,
}

impl Plan {
    /// Measured minutes per cycle.
    pub fn cycle_minutes(&self) -> u32 {
        u32::try_from(self.minutes.len()).unwrap_or(5)
    }
}

/// What the station can measure now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontEnd {
    /// `[start, end]` the station can tune now.
    pub tunable: (u64, u64),
    /// The front-end centre (the DC spike), Hz.
    pub center_hz: u64,
}

/// The measurement channel's shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    /// Bins per spectrum.
    pub bin_count: u32,
    /// Hz per bin.
    pub resolution_bw_hz: f32,
    /// Time per window, ms.
    pub dwell_ms: u32,
}

impl Shape {
    /// One window's width, Hz.
    pub fn span_hz(&self) -> u64 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "bounded by the plan's checks: at most 4096 × 10 kHz"
        )]
        let span = (f64::from(self.bin_count) * f64::from(self.resolution_bw_hz)).round() as u64;
        span.max(1)
    }

    /// Slots per minute.
    pub fn slots_per_minute(&self) -> u32 {
        (60_000 / self.dwell_ms.max(1)).max(1)
    }
}

/// The usable parts of the front end: the tunable range minus
/// [`EDGE_PERCENT`] at each end, minus ±[`DC_GUARD_HZ`] around the centre.
pub fn usable(fe: FrontEnd) -> Vec<(u64, u64)> {
    let (lo, hi) = fe.tunable;
    if lo >= hi {
        return Vec::new();
    }
    let edge = (hi - lo) * EDGE_PERCENT / 100;
    let (lo, hi) = (lo + edge, hi - edge);
    let hole = (
        fe.center_hz.saturating_sub(DC_GUARD_HZ),
        fe.center_hz.saturating_add(DC_GUARD_HZ),
    );
    let mut out = Vec::with_capacity(2);
    if lo < hole.0.min(hi) {
        out.push((lo, hole.0.min(hi)));
    }
    if hole.1.max(lo) < hi {
        out.push((hole.1.max(lo), hi));
    }
    out
}

fn intersect(a: (u64, u64), b: (u64, u64)) -> Option<(u64, u64)> {
    let r = (a.0.max(b.0), a.1.min(b.1));
    (r.0 < r.1).then_some(r)
}

/// `ranges` ∩ `usable`, sorted.
pub fn clip(ranges: &[(u64, u64)], usable: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = ranges
        .iter()
        .flat_map(|r| usable.iter().filter_map(|u| intersect(*r, *u)))
        .collect();
    out.sort_unstable();
    out
}

/// `hz` on `grid`: the nearest point, or rounded down or up.
fn snap(hz: u64, grid: Option<MeasurementGrid>, round: Round) -> u64 {
    let Some(g) = grid.filter(|g| g.grid_hz > 1) else {
        return hz;
    };
    let step = i128::from(g.grid_hz);
    let off = i128::from(hz) - i128::from(g.anchor_hz);
    let k = match round {
        Round::Nearest => (off + step / 2).div_euclid(step),
        Round::Down => off.div_euclid(step),
        Round::Up => (off + step - 1).div_euclid(step),
    };
    u64::try_from(i128::from(g.anchor_hz) + k * step).unwrap_or(hz)
}

#[derive(Debug, Clone, Copy)]
enum Round {
    Nearest,
    Down,
    Up,
}

/// The window centres that cover `[lo, hi)` with windows `span` wide, each
/// kept inside the tunable range `[t_lo, t_hi]` and on `grid`.
fn window_centres(
    lo: u64,
    hi: u64,
    span: u64,
    (t_lo, t_hi): (u64, u64),
    grid: Option<MeasurementGrid>,
) -> Vec<u64> {
    let step = grid.map_or(0, |g| g.grid_hz);
    // Snapping moves a centre by up to half a step: plan one step narrower.
    let reach = span.saturating_sub(step).max(1);
    let n = (hi - lo).div_ceil(reach).max(1);
    let half = span / 2;
    let (min_c, max_c) = if t_hi.saturating_sub(t_lo) >= span {
        (
            snap(t_lo + half, grid, Round::Up),
            snap(t_hi - (span - half), grid, Round::Down),
        )
    } else {
        let mid = snap(t_lo / 2 + t_hi / 2, grid, Round::Nearest);
        (mid, mid)
    };
    // Spread the windows evenly over the band.
    (0..n)
        .map(|k| {
            let c = lo + (hi - lo) * (2 * k + 1) / (2 * n);
            snap(c, grid, Round::Nearest).clamp(min_c, max_c.max(min_c))
        })
        .collect()
}

/// Plans `bands` (each `(band_id, key, ranges)`, in order) for front end
/// `fe`, with window centres on `grid` when the backend has one, at most
/// `max_bands`. Bands with nothing usable, beyond `max_bands`, or beyond 5
/// minutes of windows are left out.
pub fn plan(
    bands: &[BandRequest],
    fe: FrontEnd,
    shape: Shape,
    grid: Option<MeasurementGrid>,
    max_bands: usize,
) -> Plan {
    let span = shape.span_hz();
    let slots = shape.slots_per_minute() as usize;
    let usable = usable(fe);
    let mut accepted: Vec<Band> = Vec::new();
    let mut minutes: Vec<Vec<Window>> = vec![Vec::new()];
    for (band_id, key, ranges) in bands {
        if accepted.len() >= max_bands {
            break;
        }
        let mut ranges = clip(ranges, &usable);
        let (Some(first), Some(last)) = (ranges.first().copied(), ranges.last().copied()) else {
            continue;
        };
        let mut centres = window_centres(first.0, last.1, span, fe.tunable, grid);
        if centres.len() > slots {
            // Clip the band to one minute of windows.
            centres.truncate(slots);
            let end = centres.last().map_or(last.1, |c| c + span / 2);
            ranges = clip(&ranges, &[(first.0, end)]);
            if ranges.is_empty() {
                continue;
            }
        }
        let fits_current = minutes
            .last()
            .is_some_and(|m| m.len() + centres.len() <= slots);
        if !fits_current {
            if minutes.len() >= 5 {
                continue;
            }
            minutes.push(Vec::new());
        }
        let index = accepted.len();
        accepted.push(Band {
            band_id: *band_id,
            key: key.clone(),
            ranges,
        });
        if let Some(minute) = minutes.last_mut() {
            minute.extend(centres.into_iter().map(|center_hz| Window {
                center_hz,
                bands: vec![index],
            }));
        }
    }
    let needed = minutes.len();
    let cycle = CYCLE_MINUTES
        .iter()
        .map(|&c| c as usize)
        .find(|&c| c >= needed)
        .unwrap_or(5);
    minutes.resize(cycle, Vec::new());
    Plan {
        bands: accepted,
        minutes,
        slots_per_minute: shape.slots_per_minute(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPE: Shape = Shape {
        bin_count: 1024,
        resolution_bw_hz: 500.0,
        dwell_ms: 4000,
    };
    /// An RTL-SDR at 2.048 MS/s on 7.1 MHz (the packaged `rtlsdr-hf` default).
    const RTL_40M: FrontEnd = FrontEnd {
        tunable: (6_076_000, 8_124_000),
        center_hz: 7_100_000,
    };
    /// radiod's grid with 500 Hz bins, from the RTL-SDR's LO.
    const GRID_40M: MeasurementGrid = MeasurementGrid {
        anchor_hz: 7_100_000,
        grid_hz: 1_000,
    };
    /// A wideband HF receiver.
    const RX888: FrontEnd = FrontEnd {
        tunable: (0, 32_000_000),
        center_hz: 16_000_000,
    };

    fn band(id: u32, key: &str, lo: u64, hi: u64) -> BandRequest {
        (id, key.to_owned(), vec![(lo, hi)])
    }

    #[test]
    fn forty_metres_on_an_rtl_sdr_is_one_window() {
        let p = plan(
            &[band(1, "40m", 7_000_000, 7_300_000)],
            RTL_40M,
            SHAPE,
            Some(GRID_40M),
            32,
        );
        assert_eq!(p.cycle_minutes(), 1);
        assert_eq!(p.slots_per_minute, 15);
        assert_eq!(p.bands.len(), 1);
        // The DC guard splits it.
        assert_eq!(
            p.bands[0].ranges,
            vec![(7_000_000, 7_098_000), (7_102_000, 7_300_000)]
        );
        assert_eq!(p.minutes[0].len(), 1);
        assert_eq!(p.minutes[0][0].center_hz, 7_150_000);
        assert_eq!(p.minutes[0][0].bands, vec![0]);
    }

    #[test]
    fn hf_on_a_wideband_receiver_takes_thirteen_windows() {
        // The measurement design §2.2: 160 to 10 m (region 2 edges) take 13 windows.
        let hf = [
            band(1, "160m", 1_800_000, 2_000_000),
            band(2, "80m", 3_500_000, 4_000_000),
            band(3, "60m", 5_351_500, 5_366_500),
            band(4, "40m", 7_000_000, 7_300_000),
            band(5, "30m", 10_100_000, 10_150_000),
            band(6, "20m", 14_000_000, 14_350_000),
            band(7, "17m", 18_068_000, 18_168_000),
            band(8, "15m", 21_000_000, 21_450_000),
            band(9, "12m", 24_890_000, 24_990_000),
            band(10, "10m", 28_000_000, 29_700_000),
        ];
        let p = plan(&hf, RX888, SHAPE, Some(GRID_40M), 32);
        let windows: usize = p.minutes.iter().map(Vec::len).sum();
        assert_eq!(windows, 13);
        assert_eq!(p.cycle_minutes(), 1);
        assert_eq!(p.bands.len(), 10);
        // 10 m takes 4.
        let ten = p.minutes[0].iter().filter(|w| w.bands == vec![9]).count();
        assert_eq!(ten, 4);
    }

    #[test]
    fn many_windows_stretch_the_cycle() {
        // 2 m and 70 cm on a wide VHF receiver: 8 + 20 windows → 2 minutes...
        let vhf = FrontEnd {
            tunable: (100_000_000, 500_000_000),
            center_hz: 300_000_000,
        };
        let p = plan(
            &[
                band(1, "2m", 144_000_000, 148_000_000),
                band(2, "70cm", 420_000_000, 430_000_000),
            ],
            vhf,
            SHAPE,
            None,
            32,
        );
        assert_eq!(p.cycle_minutes(), 2);
        assert_eq!(p.minutes[0].len(), 8);
        // ...and 70 cm is clipped to one minute of windows (15 × 512 kHz).
        assert_eq!(p.minutes[1].len(), 15);
        let seventy = &p.bands[1].ranges;
        assert_eq!(seventy[0].0, 420_000_000);
        assert!(seventy.last().expect("range").1 <= 420_000_000 + 15 * 512_000 + 512_000);
        // Three minutes round up to five.
        let p = plan(
            &[
                band(1, "a", 144_000_000, 151_000_000),
                band(2, "b", 200_000_000, 207_000_000),
                band(3, "c", 300_000_000, 307_000_000),
            ],
            vhf,
            SHAPE,
            None,
            32,
        );
        assert_eq!(p.cycle_minutes(), 5);
        assert!(p.minutes[3].is_empty() && p.minutes[4].is_empty());
    }

    #[test]
    fn what_is_left_out() {
        // Outside the tunable range, or only in the edges or the DC guard.
        let p = plan(
            &[
                band(1, "20m", 14_000_000, 14_350_000),
                band(2, "x", 6_076_000, 6_100_000),
                band(3, "y", 7_099_000, 7_101_000),
                band(4, "40m", 7_000_000, 7_300_000),
            ],
            RTL_40M,
            SHAPE,
            None,
            32,
        );
        assert_eq!(p.bands.len(), 1);
        assert_eq!(p.bands[0].band_id, 4);
        // The local cap.
        let p = plan(
            &[
                band(1, "40m", 7_000_000, 7_300_000),
                band(2, "41m", 7_300_000, 7_400_000),
            ],
            RTL_40M,
            SHAPE,
            Some(GRID_40M),
            1,
        );
        assert_eq!(p.bands.len(), 1);
        // Nothing at all.
        let p = plan(&[], RTL_40M, SHAPE, Some(GRID_40M), 32);
        assert!(p.bands.is_empty());
        assert_eq!(p.cycle_minutes(), 1);
    }

    #[test]
    fn windows_stay_inside_the_tunable_range() {
        // A band at the top edge of the passband.
        let p = plan(
            &[band(1, "x", 7_900_000, 8_010_000)],
            RTL_40M,
            SHAPE,
            Some(GRID_40M),
            32,
        );
        let c = p.minutes[0][0].center_hz;
        assert!(c + 256_000 <= RTL_40M.tunable.1, "{c}");
        // A front end narrower than one window: the channel sits in the middle.
        let narrow = FrontEnd {
            tunable: (7_000_000, 7_200_000),
            center_hz: 7_100_000,
        };
        let p = plan(
            &[band(1, "40m", 7_000_000, 7_300_000)],
            narrow,
            SHAPE,
            Some(GRID_40M),
            32,
        );
        assert_eq!(p.minutes[0][0].center_hz, 7_100_000);
        assert_eq!(
            usable(narrow),
            vec![(7_010_000, 7_098_000), (7_102_000, 7_190_000)]
        );
        assert!(
            usable(FrontEnd {
                tunable: (5, 5),
                center_hz: 5
            })
            .is_empty()
        );
    }
}
