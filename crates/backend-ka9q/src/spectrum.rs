// SPDX-License-Identifier: GPL-3.0-only
//! radiod spectrum2 (`SPECT2_DEMOD`) rows -> our uint8 waterfall rows.
//!
//! # radiod facts (checked against the pinned upstream clone)
//!
//! - **Byte formula.** `encode_byte_data` (`src/spectrum.c:529`) fills the
//!   `BIN_BYTE_DATA` payload with, per bin:
//!   `byte = clamp(round((power2dB(power) - base) / step), 0, 255)`, where
//!   `base` = `SPECTRUM_BASE` and `step` = `SPECTRUM_STEP` (both `f32`
//!   status TLVs sent alongside the row, `src/radio_status.c:803-806`). So
//!   the inverse (what a byte *means*) is
//!   `power_db = base + byte * step`, with byte 0 the floor and byte 255
//!   the (near-)ceiling — this is exactly the `offset_db`/`step_db`
//!   semantics of `coaxhub.station.v1.Quantization`
//!   (`proto/coaxhub/station/v1/common.proto:71-74`,
//!   station-protocol.md §6.2), which is why [`Quantization`] mirrors it
//!   field-for-field.
//!   `base`/`step` themselves are chosen by radiod each row from the
//!   block's min/max power (`src/spectrum.c`, around the scaling comment
//!   `"scaling for byte bin format"`): `base = power2dB(min_power)`,
//!   `step = (power2dB(max_power) - base) / 256`.
//! - **Bin order.** `encode_byte_data` starts reading its internal,
//!   DC-first `bin_data` array (see the "KE5GDB" fix comment in
//!   `src/spectrum.c`, `offset = i < bin_count/2 ? i : i - bin_count`,
//!   i.e. bins `0..bin_count/2` are DC and positive frequencies and
//!   `bin_count/2..bin_count` are negative frequencies, most negative
//!   first — matching `status.h`'s `DEMOD_TYPE=3` "spectrum" comment
//!   "DC-first order") at `wbin = bin_count/2` (the most negative
//!   frequency) and walks forward, wrapping through DC and the positive
//!   frequencies. So the output byte array (`BIN_BYTE_DATA`, used by
//!   `SPECT2_DEMOD`) runs **low-to-high frequency**, confirming the
//!   brief's claim and `status.h`'s `DEMOD_TYPE=4` comment
//!   ("8-bit log bins, low-to-high").
//! - **Bin center frequencies.** The channel's tuned frequency
//!   (`RADIO_FREQUENCY`/`chan->tune.freq`, `status.h:62`,
//!   `src/radio_status.c:696`) is the spectrum's *center* frequency: the
//!   debug prints in `src/spectrum.c` (`"%s wide spectrum: center %.3lf Hz
//!   bin count %u, rbw %.1lf Hz ..."`, around lines 616-617 and 658-659)
//!   label `chan->tune.freq` as `center`. Combined with the bin-order
//!   fact above, output bin `i` (0-based, low-to-high) sits at the same
//!   signed FFT-bin offset from center as internal `bin_data[i]` would if
//!   it were still DC-first, i.e. `offset(i) = i - bin_count/2` (integer
//!   division, matching the C `int` arithmetic in `spectrum.c`), so
//!   `frequency_of_bin(i) = RADIO_FREQUENCY + offset(i) * RESOLUTION_BW`.
//!   `RESOLUTION_BW` (`rbw`) is sent directly as a status TLV
//!   (`src/radio_status.c:782`) — we don't need to re-derive it from
//!   `BIN_COUNT`/FFT size, we just carry it as `bin_width_hz`.
//!
//! None of the above needs to be re-derived by stationd: radiod computes
//! `base`/`step`/`rbw` and sends them as TLVs every row, so [`SpectrumFrame`]
//! just carries radiod's own numbers; this module's job is to convert them
//! into *our* wire format (a different, hub-chosen [`Quantization`]),
//! optionally crop/decimate, and compress/decompress rows.

use std::io::Read;

/// Errors from spectrum row processing.
#[derive(Debug, thiserror::Error)]
pub enum SpectrumError {
    /// A zstd (de)compression I/O error.
    #[error("zstd I/O error: {0}")]
    Zstd(#[source] std::io::Error),
    /// [`decompress_row_bounded`] got a different number of bytes than
    /// `expected_bins`, either because the row was truncated or because
    /// more data was available than expected (the zip-bomb guard capped
    /// the read before the true length was reached).
    #[error("decompressed row is {got} bytes, expected exactly {expected}")]
    RowLengthMismatch {
        /// Bytes actually produced (capped at `expected + 1`).
        got: usize,
        /// Bytes the caller declared the row should contain.
        expected: usize,
    },
    /// [`crop`] was asked for an empty or inverted range.
    #[error("crop range is empty or inverted: start_hz {start_hz} >= end_hz {end_hz}")]
    InvalidCropRange {
        /// Requested range start, Hz.
        start_hz: f64,
        /// Requested range end, Hz.
        end_hz: f64,
    },
    /// [`crop`] found no bins overlapping the requested range.
    #[error("crop range [{start_hz}, {end_hz}) Hz does not overlap the frame's bins")]
    CropOutOfRange {
        /// Requested range start, Hz.
        start_hz: f64,
        /// Requested range end, Hz.
        end_hz: f64,
    },
}

/// Our own quantization scale, mirroring `coaxhub.station.v1.Quantization`
/// (`proto/coaxhub/station/v1/common.proto`) and station-protocol.md §6.2:
/// `power_dbfs = offset_db + v * step_db`, with `v = 0` the floor and
/// `v = 255` the ceiling of the representable range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quantization {
    /// Power, in dB, that byte value 0 represents.
    pub offset_db: f32,
    /// dB per LSB.
    pub step_db: f32,
}

impl Quantization {
    /// Creates a new quantization scale.
    #[must_use]
    pub fn new(offset_db: f32, step_db: f32) -> Self {
        Quantization { offset_db, step_db }
    }

    /// Decodes a byte value to a power in dB, per station-protocol.md §6.2.
    #[must_use]
    pub fn power_dbfs(&self, v: u8) -> f32 {
        self.offset_db + f32::from(v) * self.step_db
    }

    /// Encodes a power in dB to the nearest representable byte value,
    /// saturating to `[0, 255]`. Non-finite input, or a non-positive/
    /// non-finite `step_db`, maps to 0 rather than panicking or producing
    /// NaN/garbage — this only happens on malformed input, never on data
    /// this module itself produces.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // Safe: `x` is clamped to [0.0, 255.0] immediately above, so the f32 ->
    // u8 cast never truncates a fractional part that matters or loses a
    // negative sign.
    pub fn quantize(&self, power_db: f32) -> u8 {
        if !self.step_db.is_finite() || self.step_db <= 0.0 || !power_db.is_finite() {
            return 0;
        }
        let x = (power_db - self.offset_db) / self.step_db;
        if !x.is_finite() {
            return 0;
        }
        x.round().clamp(0.0, 255.0) as u8
    }
}

/// One radiod spectrum2 row: `BIN_COUNT` 8-bit log-magnitude bins, low to
/// high frequency, plus the TLVs needed to interpret them
/// (`RADIO_FREQUENCY`, `RESOLUTION_BW`, `SPECTRUM_BASE`, `SPECTRUM_STEP`;
/// see the module docs for how radiod produces each one).
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumFrame {
    /// Center frequency of the spectrum window, Hz (`RADIO_FREQUENCY`).
    pub center_hz: f64,
    /// Width of one bin, Hz (`RESOLUTION_BW`).
    pub bin_width_hz: f64,
    /// `SPECTRUM_BASE`: power, in dB, that `bins[i] == 0` represents.
    pub base_db: f32,
    /// `SPECTRUM_STEP`: dB per LSB of `bins[i]`.
    pub step_db: f32,
    /// Bin values, low to high frequency (`BIN_BYTE_DATA`).
    pub bins: Vec<u8>,
}

impl SpectrumFrame {
    /// Frequency, in Hz, of the center of bin `i` (0-based, low to high),
    /// per the formula derived in the module docs:
    /// `center_hz + (i - bins.len() / 2) * bin_width_hz`, using truncating
    /// integer division for `bins.len() / 2` to match radiod's own C
    /// arithmetic.
    #[must_use]
    #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
    // Safe: station-protocol caps `max_bins` at 16,384, so these
    // usize -> i64 -> f64 conversions never wrap or lose precision in
    // practice, even though the types don't prove that statically.
    pub fn frequency_of_bin(&self, i: usize) -> f64 {
        let half = (self.bins.len() / 2) as i64;
        let offset = i as i64 - half;
        self.center_hz + (offset as f64) * self.bin_width_hz
    }

    /// The frequency range covered by this frame: the low edge of bin 0 to
    /// the high edge of the last bin. Returns `(center_hz, center_hz)` for
    /// an empty frame.
    #[must_use]
    pub fn range(&self) -> (f64, f64) {
        if self.bins.is_empty() {
            return (self.center_hz, self.center_hz);
        }
        let half_width = self.bin_width_hz / 2.0;
        let low = self.frequency_of_bin(0) - half_width;
        let high = self.frequency_of_bin(self.bins.len() - 1) + half_width;
        (low, high)
    }
}

/// Converts a radiod-native [`SpectrumFrame`] (quantized on `base_db`/
/// `step_db`) to our wire scale `q`, with saturation and round-to-nearest.
#[must_use]
pub fn requantize(frame: &SpectrumFrame, q: &Quantization) -> Vec<u8> {
    frame
        .bins
        .iter()
        .map(|&v| {
            let power_db = frame.base_db + f32::from(v) * frame.step_db;
            q.quantize(power_db)
        })
        .collect()
}

/// Crops `frame` to the bins whose center frequency falls in
/// `[start_hz, end_hz)`, returning a new frame with `bin_width_hz`,
/// `base_db` and `step_db` unchanged and `center_hz` recomputed so
/// [`SpectrumFrame::frequency_of_bin`] still returns the correct absolute
/// frequency for every retained bin.
///
/// # Errors
/// Returns [`SpectrumError::InvalidCropRange`] if `start_hz >= end_hz`, or
/// [`SpectrumError::CropOutOfRange`] if no bin's center falls in the
/// requested range.
pub fn crop(
    frame: &SpectrumFrame,
    start_hz: f64,
    end_hz: f64,
) -> Result<SpectrumFrame, SpectrumError> {
    // Written as a positive `matches!` on `partial_cmp` (rather than
    // `!(start_hz < end_hz)`) so NaN is correctly treated as invalid too:
    // every NaN comparison is false, so a bare `!(start_hz < end_hz)` and a
    // `start_hz >= end_hz` would both (correctly, but for the wrong
    // syntactic reason `clippy::neg_cmp_op_on_partial_ord` warns about)
    // treat NaN as invalid; this spells the intent directly instead.
    let ordered = matches!(
        start_hz.partial_cmp(&end_hz),
        Some(std::cmp::Ordering::Less)
    );
    if !ordered {
        return Err(SpectrumError::InvalidCropRange { start_hz, end_hz });
    }
    let mut lo = None;
    let mut hi = 0usize;
    for i in 0..frame.bins.len() {
        let f = frame.frequency_of_bin(i);
        if f >= start_hz && f < end_hz {
            if lo.is_none() {
                lo = Some(i);
            }
            hi = i + 1;
        }
    }
    let Some(lo) = lo else {
        return Err(SpectrumError::CropOutOfRange { start_hz, end_hz });
    };

    let new_bins = frame.bins[lo..hi].to_vec();
    // Anchor so that frequency_of_bin(0) on the new frame equals
    // frame.frequency_of_bin(lo) on the old one; see the crop() doc comment
    // above for why this construction is exact regardless of the two
    // frames' bin-count parity. Bin counts are always tiny (see
    // frequency_of_bin's doc comment), so these conversions are exact.
    #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
    let new_half = (new_bins.len() / 2) as i64;
    let f0 = frame.frequency_of_bin(lo);
    #[allow(clippy::cast_precision_loss)]
    let new_center = f0 + (new_half as f64) * frame.bin_width_hz;

    Ok(SpectrumFrame {
        center_hz: new_center,
        bin_width_hz: frame.bin_width_hz,
        base_db: frame.base_db,
        step_db: frame.step_db,
        bins: new_bins,
    })
}

/// Peak-preserving decimation (station-protocol.md §6.2): each output bin
/// is the **maximum** of the input bins it covers, so a single hot bin
/// (narrow carrier) is never averaged away.
///
/// Handles non-integer input/output ratios by partitioning the input into
/// `out_bins` contiguous, non-overlapping, monotonically increasing ranges
/// (`i * n / out_bins .. (i + 1) * n / out_bins`), so every input bin
/// contributes to exactly one output bin.
///
/// Policy for `out_bins >= bins.len()` (upsampling): returns `bins`
/// unchanged rather than erroring or interpolating, since a waterfall row
/// only ever needs to *shrink* its bin count for a listener and this keeps
/// the function total. Returns an empty vector if `bins` is empty or
/// `out_bins == 0`.
#[must_use]
pub fn peak_decimate(bins: &[u8], out_bins: usize) -> Vec<u8> {
    if bins.is_empty() || out_bins == 0 {
        return Vec::new();
    }
    let n = bins.len();
    if out_bins >= n {
        return bins.to_vec();
    }
    let mut out = Vec::with_capacity(out_bins);
    for i in 0..out_bins {
        let start = i * n / out_bins;
        let mut end = (i + 1) * n / out_bins;
        if end <= start {
            end = start + 1;
        }
        let end = end.min(n);
        let max = bins[start..end].iter().copied().max().unwrap_or(0);
        out.push(max);
    }
    out
}

/// Compresses one waterfall row with zstd (station-protocol.md §6.2: "the
/// station ... compresses each row (zstd by default)"), one independent
/// frame per row.
///
/// # Errors
/// Returns [`SpectrumError::Zstd`] if the zstd encoder fails.
pub fn compress_row(bins: &[u8], level: i32) -> Result<Vec<u8>, SpectrumError> {
    zstd::stream::encode_all(bins, level).map_err(SpectrumError::Zstd)
}

/// Decompresses one zstd-compressed waterfall row, bounded to
/// `expected_bins + 1` bytes of *output* regardless of what the compressed
/// data claims or actually contains (station-protocol.md §5's zip-bomb
/// guard: "decompression capped at `bins` + 1 bytes"). This bounds memory
/// use even against an adversarial frame that would decompress to
/// gigabytes: only `expected_bins + 1` bytes are ever pulled out of the
/// decoder.
///
/// # Errors
/// Returns [`SpectrumError::RowLengthMismatch`] unless decompression
/// produces **exactly** `expected_bins` bytes (too few: truncated/invalid
/// row; `expected_bins + 1`: more data was available than declared, i.e.
/// the guard tripped). Returns [`SpectrumError::Zstd`] on a malformed zstd
/// stream.
pub fn decompress_row_bounded(
    compressed: &[u8],
    expected_bins: usize,
) -> Result<Vec<u8>, SpectrumError> {
    let decoder = zstd::stream::read::Decoder::new(compressed).map_err(SpectrumError::Zstd)?;
    let cap = expected_bins as u64 + 1;
    let mut limited = decoder.take(cap);
    let mut out = Vec::new();
    limited.read_to_end(&mut out).map_err(SpectrumError::Zstd)?;
    if out.len() != expected_bins {
        return Err(SpectrumError::RowLengthMismatch {
            got: out.len(),
            expected: expected_bins,
        });
    }
    Ok(out)
}

#[cfg(test)]
// Test assertions compare floats computed from exact integer/half-integer
// arithmetic (no rounding error is possible for the values used), so exact
// equality is intentional. `naive_bytecount`/`cast_lossless` are style
// lints not worth restructuring simple test code around.
#[allow(clippy::float_cmp, clippy::naive_bytecount, clippy::cast_lossless)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn frame(center_hz: f64, bin_width_hz: f64, bins: Vec<u8>) -> SpectrumFrame {
        SpectrumFrame {
            center_hz,
            bin_width_hz,
            base_db: -160.0,
            step_db: 0.5,
            bins,
        }
    }

    // --- Quantization ---

    #[test]
    fn quantize_floor_is_zero() {
        let q = Quantization::new(-160.0, 0.5);
        assert_eq!(q.quantize(-160.0), 0);
        assert_eq!(q.power_dbfs(0), -160.0);
    }

    #[test]
    fn quantize_ceiling_is_255() {
        let q = Quantization::new(-160.0, 0.5);
        // offset + 255*step = -160 + 127.5 = -32.5
        assert_eq!(q.quantize(-32.5), 255);
        assert_eq!(q.power_dbfs(255), -32.5);
    }

    #[test]
    fn quantize_saturates_below_and_above() {
        let q = Quantization::new(-160.0, 0.5);
        assert_eq!(q.quantize(-1000.0), 0);
        assert_eq!(q.quantize(1000.0), 255);
    }

    #[test]
    fn quantize_rounds_to_nearest() {
        let q = Quantization::new(0.0, 1.0);
        assert_eq!(q.quantize(2.4), 2);
        assert_eq!(q.quantize(2.5), 3); // round-half-away-from-zero
        assert_eq!(q.quantize(2.6), 3);
    }

    #[test]
    fn quantize_handles_degenerate_step() {
        let q = Quantization::new(0.0, 0.0);
        assert_eq!(q.quantize(5.0), 0);
        let q2 = Quantization::new(0.0, -1.0);
        assert_eq!(q2.quantize(5.0), 0);
        let q3 = Quantization::new(0.0, f32::NAN);
        assert_eq!(q3.quantize(5.0), 0);
    }

    #[test]
    fn quantize_handles_nonfinite_input() {
        let q = Quantization::new(0.0, 1.0);
        assert_eq!(q.quantize(f32::NAN), 0);
        assert_eq!(q.quantize(f32::INFINITY), 0);
    }

    proptest! {
        #[test]
        fn quantize_never_panics(offset in -300.0f32..300.0, step in -10.0f32..10.0, power in -1000.0f32..1000.0) {
            let q = Quantization::new(offset, step);
            let _ = q.quantize(power);
        }

        #[test]
        fn quantize_output_in_range(offset in -300.0f32..300.0, step in 0.001f32..10.0, power in -1000.0f32..1000.0) {
            let q = Quantization::new(offset, step);
            let v = q.quantize(power);
            prop_assert!((0..=255).contains(&(v as i32)));
        }
    }

    // --- SpectrumFrame::frequency_of_bin / range ---

    #[test]
    fn frequency_of_bin_matches_radiod_formula_even_count() {
        // bin_count = 8, center 1_000_000 Hz, rbw 100 Hz.
        // bin 0 -> offset -4 -> 999_600; bin 4 (DC) -> offset 0 -> 1_000_000; bin 7 -> offset 3 -> 1_000_300
        let f = frame(1_000_000.0, 100.0, vec![0; 8]);
        assert_eq!(f.frequency_of_bin(0), 999_600.0);
        assert_eq!(f.frequency_of_bin(4), 1_000_000.0);
        assert_eq!(f.frequency_of_bin(7), 1_000_300.0);
    }

    #[test]
    fn range_covers_bin_edges() {
        let f = frame(1_000_000.0, 100.0, vec![0; 8]);
        let (lo, hi) = f.range();
        assert_eq!(lo, 999_600.0 - 50.0);
        assert_eq!(hi, 1_000_300.0 + 50.0);
    }

    #[test]
    fn range_of_empty_frame_is_degenerate() {
        let f = frame(42.0, 100.0, vec![]);
        assert_eq!(f.range(), (42.0, 42.0));
    }

    // --- requantize ---

    #[test]
    fn requantize_identity_when_scales_match() {
        let f = frame(0.0, 1.0, vec![0, 1, 128, 255]);
        let q = Quantization::new(f.base_db, f.step_db);
        assert_eq!(requantize(&f, &q), vec![0, 1, 128, 255]);
    }

    #[test]
    fn requantize_rescales() {
        // radiod base=-160,step=0.5: byte 200 -> -60 dB.
        let f = frame(0.0, 1.0, vec![200]);
        // Our scale: offset=-100, step=1.0 dB -> -60 dB -> v=40.
        let q = Quantization::new(-100.0, 1.0);
        assert_eq!(requantize(&f, &q), vec![40]);
    }

    // --- crop ---

    #[test]
    fn crop_selects_expected_bins_and_recenters() {
        // 8 bins, center 1_000_000, rbw 100: bins at 999_600..1_000_300 step 100.
        let f = frame(1_000_000.0, 100.0, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        // crop to [999_950, 1_000_250) should select bins whose centers are
        // 1_000_000 (bin 4) and 1_000_100 (bin 5), and 1_000_200 (bin 6) is
        // excluded since 1_000_200 < 1_000_250 is true... check formula.
        let cropped = crop(&f, 999_950.0, 1_000_250.0).unwrap();
        assert_eq!(cropped.bins, vec![4, 5, 6]);
        assert_eq!(cropped.frequency_of_bin(0), 1_000_000.0);
        assert_eq!(cropped.frequency_of_bin(1), 1_000_100.0);
        assert_eq!(cropped.frequency_of_bin(2), 1_000_200.0);
    }

    #[test]
    fn crop_single_bin() {
        let f = frame(1_000_000.0, 100.0, vec![10, 20, 30, 40]);
        // centers: 999_800, 999_900, 1_000_000, 1_000_100
        let cropped = crop(&f, 999_950.0, 1_000_050.0).unwrap();
        assert_eq!(cropped.bins, vec![30]);
        assert_eq!(cropped.frequency_of_bin(0), 1_000_000.0);
    }

    #[test]
    fn crop_full_range_is_identity() {
        let f = frame(1_000_000.0, 100.0, vec![1, 2, 3, 4, 5]);
        let (lo, hi) = f.range();
        let cropped = crop(&f, lo, hi + 1.0).unwrap();
        assert_eq!(cropped.bins, f.bins);
        assert_eq!(cropped.center_hz, f.center_hz);
    }

    #[test]
    fn crop_rejects_inverted_range() {
        let f = frame(0.0, 1.0, vec![1, 2, 3]);
        assert!(matches!(
            crop(&f, 10.0, 5.0),
            Err(SpectrumError::InvalidCropRange { .. })
        ));
        assert!(matches!(
            crop(&f, 5.0, 5.0),
            Err(SpectrumError::InvalidCropRange { .. })
        ));
    }

    #[test]
    fn crop_rejects_out_of_range() {
        let f = frame(1_000_000.0, 100.0, vec![1, 2, 3, 4]);
        let (_, hi) = f.range();
        assert!(matches!(
            crop(&f, hi + 1000.0, hi + 2000.0),
            Err(SpectrumError::CropOutOfRange { .. })
        ));
    }

    // --- peak_decimate ---

    #[test]
    fn peak_decimate_integer_ratio() {
        let bins: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let out = peak_decimate(&bins, 4);
        assert_eq!(out, vec![2, 4, 6, 8]);
    }

    #[test]
    fn peak_decimate_fractional_ratio() {
        // 10 bins -> 3 output bins: partitions of size ~3.33 (4,3,3 by floor rule).
        let bins: Vec<u8> = (1..=10).collect();
        let out = peak_decimate(&bins, 3);
        assert_eq!(out.len(), 3);
        // boundaries: 0,3,6,10 -> [1,2,3]max=3 [4,5,6]max=6 [7,8,9,10]max=10
        assert_eq!(out, vec![3, 6, 10]);
    }

    #[test]
    fn peak_decimate_preserves_single_hot_bin() {
        let mut bins = vec![0u8; 100];
        bins[37] = 255;
        let out = peak_decimate(&bins, 10);
        assert!(out.contains(&255));
        assert_eq!(out.iter().filter(|&&v| v == 255).count(), 1);
    }

    #[test]
    fn peak_decimate_out_bins_ge_input_returns_input() {
        let bins = vec![1u8, 2, 3];
        assert_eq!(peak_decimate(&bins, 3), bins);
        assert_eq!(peak_decimate(&bins, 10), bins);
    }

    #[test]
    fn peak_decimate_empty_input() {
        assert_eq!(peak_decimate(&[], 5), Vec::<u8>::new());
        assert_eq!(peak_decimate(&[1, 2, 3], 0), Vec::<u8>::new());
    }

    proptest! {
        #[test]
        fn peak_decimate_never_panics(bins in prop::collection::vec(any::<u8>(), 0..300), out_bins in 0usize..300) {
            let _ = peak_decimate(&bins, out_bins);
        }

        #[test]
        fn peak_decimate_output_length(bins in prop::collection::vec(any::<u8>(), 1..300), out_bins in 1usize..300) {
            let out = peak_decimate(&bins, out_bins);
            let expected_len = if out_bins >= bins.len() { bins.len() } else { out_bins };
            prop_assert_eq!(out.len(), expected_len);
        }

        #[test]
        fn peak_decimate_max_preserved(bins in prop::collection::vec(any::<u8>(), 1..300), out_bins in 1usize..300) {
            let out = peak_decimate(&bins, out_bins);
            let global_max = bins.iter().copied().max().unwrap();
            let out_max = out.iter().copied().max().unwrap();
            prop_assert_eq!(out_max, global_max);
        }
    }

    // --- compress_row / decompress_row_bounded ---

    #[test]
    fn compress_decompress_round_trip() {
        let bins: Vec<u8> = (0..=255).collect();
        let compressed = compress_row(&bins, 3).unwrap();
        let decompressed = decompress_row_bounded(&compressed, bins.len()).unwrap();
        assert_eq!(decompressed, bins);
    }

    #[test]
    fn decompress_rejects_wrong_expected_length() {
        let bins = vec![7u8; 50];
        let compressed = compress_row(&bins, 3).unwrap();
        let err = decompress_row_bounded(&compressed, 49).unwrap_err();
        assert!(matches!(
            err,
            SpectrumError::RowLengthMismatch {
                got: 50,
                expected: 49
            }
        ));
    }

    #[test]
    fn decompress_bomb_guard_caps_output() {
        // A few MB of zeros compresses to a tiny blob but "wants" to expand
        // to far more than expected_bins bytes; the guard must reject it
        // without allocating the full size.
        let huge = vec![0u8; 8_000_000];
        let compressed = compress_row(&huge, 3).unwrap();
        assert!(
            compressed.len() < 10_000,
            "expected high compression ratio for an all-zero row"
        );
        let err = decompress_row_bounded(&compressed, 64).unwrap_err();
        match err {
            SpectrumError::RowLengthMismatch { got, expected } => {
                assert_eq!(expected, 64);
                assert!(
                    got <= 65,
                    "bomb guard must cap output at expected+1 bytes, got {got}"
                );
            }
            other => panic!("expected RowLengthMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decompress_rejects_malformed_stream() {
        let err = decompress_row_bounded(&[0xff, 0xff, 0xff, 0xff], 4).unwrap_err();
        assert!(matches!(err, SpectrumError::Zstd(_)));
    }

    proptest! {
        #[test]
        fn compress_decompress_round_trip_arbitrary(bins in prop::collection::vec(any::<u8>(), 0..2000)) {
            let compressed = compress_row(&bins, 1).unwrap();
            let decompressed = decompress_row_bounded(&compressed, bins.len()).unwrap();
            prop_assert_eq!(decompressed, bins);
        }
    }
}
