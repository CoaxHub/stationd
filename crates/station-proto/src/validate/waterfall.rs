// SPDX-License-Identifier: MIT
//! §5 `WaterfallRow`: bounded decompression (zip-bomb guard).
//!
//! The row is decoded in one pass (`ZSTD_decompressDCtx`) straight into a
//! preallocated buffer of exactly `bins + 1` bytes. The decoder can never
//! write more than that, and single-pass decoding allocates no window buffer,
//! so neither output size nor the frame's declared window size can make the
//! hub allocate more than `bins + 1` bytes plus one reusable decoder context,
//! whatever the station sends. (A streaming decoder with `take(bins + 1)`
//! bounds the output too, but allocates the window the frame header asks
//! for, up to 128 MiB by default.)

use zstd::zstd_safe;

use super::ViolationKind;
use super::fields::bounds;
use crate::limits;
use crate::v1::WaterfallCompression;

/// A reusable bounded waterfall-row decoder (one zstd decompression context).
pub struct WaterfallDecoder {
    dctx: Option<zstd_safe::DCtx<'static>>,
}

impl std::fmt::Debug for WaterfallDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaterfallDecoder").finish_non_exhaustive()
    }
}

impl Default for WaterfallDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl WaterfallDecoder {
    /// A decoder; the zstd context is allocated on first use.
    pub fn new() -> Self {
        Self { dctx: None }
    }

    /// Decodes one row into `out`, which on success holds exactly `bins`
    /// bytes.
    ///
    /// - `compression` is the raw `WaterfallSubscribed.compression`; `NONE`
    ///   means the payload is the row itself.
    /// - `bins` must be at most [`limits::MAX_BINS`].
    /// - The payload must be at most [`limits::MAX_WATERFALL_COMPRESSED`].
    ///
    /// Never writes more than `bins + 1` bytes into `out`.
    pub fn decode(
        &mut self,
        payload: &[u8],
        compression: i32,
        bins: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), ViolationKind> {
        out.clear();
        if bins > limits::MAX_BINS {
            return Err(bounds("waterfall_subscribed.bins"));
        }
        let bins = bins as usize;
        if payload.len() > limits::MAX_WATERFALL_COMPRESSED {
            return Err(ViolationKind::WaterfallPayloadTooLarge { len: payload.len() });
        }
        match WaterfallCompression::try_from(compression) {
            Ok(WaterfallCompression::None) => {
                if payload.len() != bins {
                    return Err(ViolationKind::WaterfallLength {
                        expected: bins,
                        actual: payload.len().min(bins + 1),
                    });
                }
                out.extend_from_slice(payload);
                Ok(())
            }
            Ok(WaterfallCompression::Zstd) => self.decode_zstd(payload, bins, out),
            Ok(WaterfallCompression::Unspecified) => Err(ViolationKind::UnspecifiedEnum {
                field: "waterfall_subscribed.compression",
            }),
            Err(_) => Err(ViolationKind::UnknownEnumValue {
                field: "waterfall_subscribed.compression",
                value: compression,
                minor: crate::PROTOCOL_VERSION.minor,
            }),
        }
    }

    fn decode_zstd(
        &mut self,
        payload: &[u8],
        bins: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), ViolationKind> {
        let cap = bins + 1;
        // Cheap early exit when the frame header declares a size.
        if let Ok(Some(declared)) = zstd_safe::get_frame_content_size(payload) {
            if declared != bins as u64 {
                return Err(ViolationKind::WaterfallLength {
                    expected: bins,
                    actual: usize::try_from(declared).unwrap_or(cap).min(cap),
                });
            }
        }
        if self.dctx.is_none() {
            self.dctx = zstd_safe::DCtx::try_create();
        }
        let Some(dctx) = self.dctx.as_mut() else {
            // Could not allocate a context: treat the row as undecodable.
            return Err(ViolationKind::WaterfallDecompression);
        };
        // Exact reservation: the buffer never grows past the largest
        // `bins + 1` seen (at most `MAX_BINS + 1`).
        out.reserve_exact(cap);
        out.resize(cap, 0);
        match dctx.decompress(&mut out[..cap], payload) {
            Ok(n) if n == bins => {
                out.truncate(bins);
                Ok(())
            }
            Ok(n) => {
                out.clear();
                Err(ViolationKind::WaterfallLength {
                    expected: bins,
                    actual: n.min(cap),
                })
            }
            Err(code) => {
                out.clear();
                // The session may be reused after an error; start clean.
                self.dctx = None;
                if zstd_safe::get_error_name(code) == "Destination buffer is too small" {
                    Err(ViolationKind::WaterfallLength {
                        expected: bins,
                        actual: cap,
                    })
                } else {
                    Err(ViolationKind::WaterfallDecompression)
                }
            }
        }
    }
}

/// One-shot [`WaterfallDecoder::decode`] returning the row. Prefer a reused
/// [`WaterfallDecoder`] on hot paths.
pub fn decompress_row(
    payload: &[u8],
    compression: i32,
    bins: u32,
) -> Result<Vec<u8>, ViolationKind> {
    let mut out = Vec::new();
    WaterfallDecoder::new().decode(payload, compression, bins, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZSTD: i32 = WaterfallCompression::Zstd as i32;
    const NONE: i32 = WaterfallCompression::None as i32;

    fn row(n: usize) -> Vec<u8> {
        (0..n).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
    }

    #[test]
    fn zstd_exact_bins_ok() {
        for n in [64usize, 1000, 16_384] {
            let r = row(n);
            let c = zstd::bulk::compress(&r, 3).expect("compress");
            let bins = u32::try_from(n).expect("bins");
            assert_eq!(decompress_row(&c, ZSTD, bins).expect("ok"), r);
            // Streaming encoder (no content size in the header) works too.
            let s = zstd::stream::encode_all(&r[..], 3).expect("encode");
            assert_eq!(decompress_row(&s, ZSTD, bins).expect("ok"), r);
        }
    }

    #[test]
    fn zstd_wrong_length() {
        let c = zstd::bulk::compress(&row(100), 3).expect("compress");
        assert_eq!(
            decompress_row(&c, ZSTD, 101),
            Err(ViolationKind::WaterfallLength {
                expected: 101,
                actual: 100
            })
        );
        assert_eq!(
            decompress_row(&c, ZSTD, 99),
            Err(ViolationKind::WaterfallLength {
                expected: 99,
                actual: 100
            })
        );
        // Without a declared size, the decoder itself finds the mismatch.
        let s = zstd::stream::encode_all(&row(100)[..], 3).expect("encode");
        assert_eq!(
            decompress_row(&s, ZSTD, 101),
            Err(ViolationKind::WaterfallLength {
                expected: 101,
                actual: 100
            })
        );
        assert_eq!(
            decompress_row(&s, ZSTD, 99),
            Err(ViolationKind::WaterfallLength {
                expected: 99,
                actual: 100
            })
        );
    }

    #[test]
    fn zip_bomb_is_capped() {
        // 64 MiB of zeros compresses to a few KiB; the header declares it.
        let big = vec![0u8; 64 << 20];
        let bomb = zstd::bulk::compress(&big, 19).expect("compress");
        assert!(bomb.len() <= limits::MAX_WATERFALL_COMPRESSED);
        assert_eq!(
            decompress_row(&bomb, ZSTD, 1024),
            Err(ViolationKind::WaterfallLength {
                expected: 1024,
                actual: 1025
            })
        );
        // Same bomb without a content-size header: decoding stops at bins + 1.
        let stream_bomb = zstd::stream::encode_all(&big[..], 19).expect("encode");
        assert!(stream_bomb.len() <= limits::MAX_WATERFALL_COMPRESSED);
        let mut dec = WaterfallDecoder::new();
        let mut out = Vec::new();
        assert_eq!(
            dec.decode(&stream_bomb, ZSTD, 1024, &mut out),
            Err(ViolationKind::WaterfallLength {
                expected: 1024,
                actual: 1025
            })
        );
        assert!(out.capacity() <= 4096, "decoder must not grow the buffer");
        // The decoder recovers for the next row.
        let ok = zstd::bulk::compress(&row(1024), 3).expect("compress");
        dec.decode(&ok, ZSTD, 1024, &mut out).expect("ok");
        assert_eq!(out, row(1024));
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(
            decompress_row(b"not zstd at all", ZSTD, 64),
            Err(ViolationKind::WaterfallDecompression)
        );
        // An empty payload is zero frames: zero bytes.
        assert_eq!(
            decompress_row(&[], ZSTD, 64),
            Err(ViolationKind::WaterfallLength {
                expected: 64,
                actual: 0
            })
        );
    }

    #[test]
    fn uncompressed_rows() {
        assert_eq!(decompress_row(&row(64), NONE, 64).expect("ok"), row(64));
        assert_eq!(
            decompress_row(&row(65), NONE, 64),
            Err(ViolationKind::WaterfallLength {
                expected: 64,
                actual: 65
            })
        );
        assert_eq!(
            decompress_row(&row(63), NONE, 64),
            Err(ViolationKind::WaterfallLength {
                expected: 64,
                actual: 63
            })
        );
    }

    #[test]
    fn parameter_guards() {
        assert_eq!(
            decompress_row(&row(64), NONE, limits::MAX_BINS + 1),
            Err(ViolationKind::OutOfBounds {
                field: "waterfall_subscribed.bins"
            })
        );
        assert_eq!(
            decompress_row(&row(16_385), NONE, 64),
            Err(ViolationKind::WaterfallPayloadTooLarge { len: 16_385 })
        );
        assert!(matches!(
            decompress_row(&row(64), 0, 64),
            Err(ViolationKind::UnspecifiedEnum { .. })
        ));
        assert!(matches!(
            decompress_row(&row(64), 99, 64),
            Err(ViolationKind::UnknownEnumValue { value: 99, .. })
        ));
        assert!(format!("{:?}", WaterfallDecoder::default()).contains("WaterfallDecoder"));
    }
}
