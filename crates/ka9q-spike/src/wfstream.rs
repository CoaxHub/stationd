// SPDX-License-Identifier: GPL-3.0-only
//! Waterfall dump files written by `ka9q-spike waterfall`.
//!
//! # Row stream (`--out`)
//!
//! All integers little-endian.
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 4 | magic `CXWF` |
//! | 4 | 1 | version (1) |
//! | 5 | 3 | reserved, 0 |
//! | 8 | 4 | `bins`: bins per row (u32) |
//! | 12 | 8 | center frequency, Hz (f64) |
//! | 20 | 8 | bin width, Hz (f64) |
//! | 28 | 4 | quantization `offset_db` (f32) |
//! | 32 | 4 | quantization `step_db` (f32) |
//!
//! Then one record per row: `t_us` (u64, microseconds since the channel was
//! opened), `len` (u32), and `len` bytes: the row's `bins` bytes compressed
//! as one zstd frame (`backend_ka9q::spectrum::compress_row`). A byte `v`
//! means `offset_db + v * step_db` dB (station-protocol §6.2).
//!
//! # Image (`--pgm`)
//!
//! A binary PGM (`P5`), one pixel row per waterfall row, oldest at the top.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};

/// File magic.
pub const MAGIC: &[u8; 4] = b"CXWF";
/// Format version.
pub const VERSION: u8 = 1;
/// Header length in bytes.
pub const HEADER_LEN: usize = 36;
/// Largest bins per row accepted by the reader.
pub const MAX_BINS: u32 = 65_536;

/// The stream header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Header {
    /// Bins per row.
    pub bins: u32,
    /// Center frequency, Hz.
    pub center_hz: f64,
    /// Bin width, Hz.
    pub bin_width_hz: f64,
    /// dB of byte value 0.
    pub offset_db: f32,
    /// dB per step.
    pub step_db: f32,
}

impl Header {
    /// Frequency of the center of bin `i` (low to high, center at
    /// `bins / 2`), as `SpectrumFrame::frequency_of_bin`.
    #[allow(clippy::cast_precision_loss, reason = "bin indices are far below 2^52")]
    pub fn frequency_of_bin(&self, i: u32) -> f64 {
        let offset = f64::from(i) - f64::from(self.bins / 2);
        self.center_hz + offset * self.bin_width_hz
    }
}

/// One row record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Microseconds since the channel was opened.
    pub t_us: u64,
    /// The zstd frame.
    pub compressed: Vec<u8>,
}

/// Writes a row stream.
#[derive(Debug)]
pub struct Writer<W: Write> {
    inner: W,
    /// Bytes written, including the header.
    pub bytes_written: u64,
}

impl<W: Write> Writer<W> {
    /// Writes the header.
    pub fn new(mut inner: W, h: &Header) -> Result<Self> {
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        buf.extend_from_slice(&[0; 3]);
        buf.extend_from_slice(&h.bins.to_le_bytes());
        buf.extend_from_slice(&h.center_hz.to_le_bytes());
        buf.extend_from_slice(&h.bin_width_hz.to_le_bytes());
        buf.extend_from_slice(&h.offset_db.to_le_bytes());
        buf.extend_from_slice(&h.step_db.to_le_bytes());
        debug_assert_eq!(buf.len(), HEADER_LEN);
        inner.write_all(&buf).context("writing header")?;
        Ok(Self {
            inner,
            bytes_written: HEADER_LEN as u64,
        })
    }

    /// Appends one row.
    pub fn write_row(&mut self, t_us: u64, compressed: &[u8]) -> Result<()> {
        let len = u32::try_from(compressed.len()).context("row too large")?;
        self.inner.write_all(&t_us.to_le_bytes())?;
        self.inner.write_all(&len.to_le_bytes())?;
        self.inner.write_all(compressed)?;
        self.bytes_written += 12 + u64::from(len);
        Ok(())
    }

    /// Flushes and returns the sink.
    pub fn finish(mut self) -> Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// Reads a row stream.
#[derive(Debug)]
pub struct Reader<R: Read> {
    inner: R,
    /// The header.
    pub header: Header,
}

fn take<const N: usize>(buf: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[at..at + N]);
    out
}

impl<R: Read> Reader<R> {
    /// Reads and checks the header.
    pub fn new(mut inner: R) -> Result<Self> {
        let mut buf = [0u8; HEADER_LEN];
        inner.read_exact(&mut buf).context("reading header")?;
        if &buf[..4] != MAGIC {
            bail!("not a CXWF file");
        }
        if buf[4] != VERSION {
            bail!("unsupported CXWF version {}", buf[4]);
        }
        let header = Header {
            bins: u32::from_le_bytes(take(&buf, 8)),
            center_hz: f64::from_le_bytes(take(&buf, 12)),
            bin_width_hz: f64::from_le_bytes(take(&buf, 20)),
            offset_db: f32::from_le_bytes(take(&buf, 28)),
            step_db: f32::from_le_bytes(take(&buf, 32)),
        };
        if header.bins == 0 || header.bins > MAX_BINS {
            bail!("bad bin count {}", header.bins);
        }
        Ok(Self { inner, header })
    }

    /// The largest compressed row accepted: zstd's worst case for `bins`
    /// bytes is well under twice the input plus a small constant.
    fn max_row_len(&self) -> usize {
        usize::try_from(self.header.bins).unwrap_or(0) * 2 + 1024
    }

    /// The next row; `None` at a clean end of file.
    pub fn next_row(&mut self) -> Result<Option<Row>> {
        let mut head = [0u8; 12];
        match self.inner.read_exact(&mut head[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        self.inner
            .read_exact(&mut head[1..])
            .context("truncated row header")?;
        let t_us = u64::from_le_bytes(take(&head, 0));
        let len = usize::try_from(u32::from_le_bytes(take(&head, 8)))?;
        if len > self.max_row_len() {
            bail!("row of {len} bytes exceeds the limit");
        }
        let mut compressed = vec![0u8; len];
        self.inner
            .read_exact(&mut compressed)
            .context("truncated row")?;
        Ok(Some(Row { t_us, compressed }))
    }
}

/// Writes a binary PGM image of `rows`, each `width` bytes. Rows of another
/// width are skipped. Returns the number of rows written.
pub fn write_pgm<W: Write>(mut w: W, width: usize, rows: &[Vec<u8>]) -> Result<usize> {
    if width == 0 {
        bail!("PGM width must be > 0");
    }
    let good: Vec<&Vec<u8>> = rows.iter().filter(|r| r.len() == width).collect();
    write!(w, "P5\n{width} {}\n255\n", good.len())?;
    for r in &good {
        w.write_all(r)?;
    }
    w.flush()?;
    Ok(good.len())
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "values round-trip bit-exactly")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn header() -> Header {
        Header {
            bins: 4,
            center_hz: 10e6,
            bin_width_hz: 1000.0,
            offset_db: -160.0,
            step_db: 0.5,
        }
    }

    #[test]
    fn round_trip() {
        let mut w = Writer::new(Vec::new(), &header()).unwrap();
        w.write_row(0, &[1, 2, 3]).unwrap();
        w.write_row(100_000, &[]).unwrap();
        assert_eq!(w.bytes_written, 36 + 15 + 12);
        let bytes = w.finish().unwrap();
        let mut r = Reader::new(bytes.as_slice()).unwrap();
        assert_eq!(r.header, header());
        assert_eq!(
            r.next_row().unwrap(),
            Some(Row {
                t_us: 0,
                compressed: vec![1, 2, 3]
            })
        );
        assert_eq!(r.next_row().unwrap().unwrap().t_us, 100_000);
        assert_eq!(r.next_row().unwrap(), None);
    }

    #[test]
    fn with_real_zstd_rows() {
        let row: Vec<u8> = (0..1024u32).map(|i| u8::try_from(i % 7).unwrap()).collect();
        let c = backend_ka9q::spectrum::compress_row(&row, 3).unwrap();
        let h = Header {
            bins: 1024,
            ..header()
        };
        let mut w = Writer::new(Vec::new(), &h).unwrap();
        w.write_row(5, &c).unwrap();
        let bytes = w.finish().unwrap();
        let mut r = Reader::new(bytes.as_slice()).unwrap();
        let got = r.next_row().unwrap().unwrap();
        let back = backend_ka9q::spectrum::decompress_row_bounded(&got.compressed, 1024).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn bin_frequencies() {
        let h = header();
        assert_eq!(h.frequency_of_bin(2), 10e6);
        assert_eq!(h.frequency_of_bin(0), 10e6 - 2000.0);
        assert_eq!(h.frequency_of_bin(3), 10e6 + 1000.0);
    }

    #[test]
    fn rejects_bad_files() {
        assert!(Reader::new(&b"CXWF"[..]).is_err());
        let good = Writer::new(Vec::new(), &header())
            .unwrap()
            .finish()
            .unwrap();
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(Reader::new(bad_magic.as_slice()).is_err());
        let mut bad_version = good.clone();
        bad_version[4] = 9;
        assert!(Reader::new(bad_version.as_slice()).is_err());
        let zero_bins = Writer::new(
            Vec::new(),
            &Header {
                bins: 0,
                ..header()
            },
        )
        .unwrap()
        .finish()
        .unwrap();
        assert!(Reader::new(zero_bins.as_slice()).is_err());

        // Truncated row header, oversized row, truncated row body.
        let mut t = good.clone();
        t.extend_from_slice(&[0; 5]);
        assert!(Reader::new(t.as_slice()).unwrap().next_row().is_err());
        let mut big = good.clone();
        big.extend_from_slice(&0u64.to_le_bytes());
        big.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Reader::new(big.as_slice()).unwrap().next_row().is_err());
        let mut short = good;
        short.extend_from_slice(&0u64.to_le_bytes());
        short.extend_from_slice(&10u32.to_le_bytes());
        short.extend_from_slice(&[1, 2]);
        assert!(Reader::new(short.as_slice()).unwrap().next_row().is_err());
    }

    #[test]
    fn pgm_output() {
        let rows = vec![vec![0, 255], vec![1], vec![7, 8]];
        let mut out = Vec::new();
        assert_eq!(write_pgm(&mut out, 2, &rows).unwrap(), 2);
        assert_eq!(out, b"P5\n2 2\n255\n\x00\xff\x07\x08");
        assert!(write_pgm(Vec::new(), 0, &rows).is_err());
    }

    proptest! {
        #[test]
        fn reader_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let mut data = Writer::new(Vec::new(), &header()).unwrap().finish().unwrap();
            data.extend_from_slice(&bytes);
            if let Ok(mut r) = Reader::new(data.as_slice()) {
                for _ in 0..64 {
                    match r.next_row() {
                        Ok(Some(_)) => {}
                        _ => break,
                    }
                }
            }
            let _ = Reader::new(bytes.as_slice());
        }
    }
}
