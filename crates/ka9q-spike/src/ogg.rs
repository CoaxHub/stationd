// SPDX-License-Identifier: GPL-3.0-only
//! Minimal, correct Ogg Opus writer: RFC 3533 Ogg framing plus RFC 7845 Ogg
//! Opus mapping, just enough to turn a sequence of already-encoded Opus
//! packets (as received over RTP from radiod) into a playable `.opus` file.
//!
//! This module does not depend on `tlv`/`command`/`status`/`tags`
//! (agent A's files) or on `backend-ka9q`'s `rtp`/`spectrum` modules (this
//! agent's other files): it only consumes raw Opus packet bytes and a
//! `Write` sink.

use std::io::Write;

use anyhow::{Context, Result, bail};

/// Parameters for the Ogg Opus `OpusHead` identification header
/// (RFC 7845 §5.1).
#[derive(Debug, Clone, Copy)]
pub struct OpusHeadParams {
    /// Output channel count. The spike only ever produces mono (radiod's
    /// Opus output is mono; see `backend_ka9q::rtp` module docs).
    pub channels: u8,
    /// Number of samples (at 48 kHz) to discard from the start of decoded
    /// output (RFC 7845 §4.2 "Pre-skip").
    ///
    /// We use 312 samples (6.5 ms) as a fixed placeholder: this is the
    /// pre-skip commonly reported by libopus-based encoders (including
    /// `opusenc`) for their default encoder configuration, i.e. a
    /// reasonable value for "some algorithmic delay was applied upstream"
    /// even though we don't actually know radiod's encoder delay (radiod's
    /// TLV protocol doesn't expose it, and this spike doesn't run its own
    /// Opus encoder — packets arrive already encoded via RTP). Per
    /// RFC 7845 §4.2, a slightly-wrong pre-skip only costs a few
    /// milliseconds of extra silence or truncation at the start of
    /// playback, not decoding errors, so a fixed reasonable default is
    /// acceptable for a spike. A production stationd should carry the
    /// real value through if/when radiod exposes it.
    pub pre_skip: u16,
    /// Sample rate of the *original* input to whatever encoded this Opus
    /// stream, Hz. Opus itself always runs its RTP/decoding clock at
    /// 48 kHz regardless of this value (RFC 7845 §5.1); it's informational
    /// metadata for players.
    pub input_sample_rate: u32,
    /// Output gain, Q7.8 fixed point dB, applied by compliant players.
    pub output_gain: i16,
}

/// Vendor string written into the `OpusTags` comment header.
const VENDOR: &[u8] = b"coaxhub ka9q-spike";

/// How much audio (at the 48 kHz Opus clock) to buffer into a page before
/// flushing it, absent an earlier flush forced by the 255-segment limit.
/// One second is a reasonable balance between page overhead and latency
/// for a spike; nothing downstream depends on this exact value.
const MAX_PAGE_SAMPLES: u64 = 48_000;

/// A page holds at most 255 lacing/segment-table entries (RFC 3533 §6). We
/// never split a single packet's lacing across page boundaries (see
/// [`OggOpusWriter::write_packet`]), which is always possible because no
/// Opus packet we ever see exceeds station-protocol's 1275-byte cap (at
/// most 6 segments) — nowhere near this budget.
const MAX_PAGE_SEGMENTS: usize = 255;

/// Writes a single logical Ogg Opus stream (RFC 3533 framing + RFC 7845
/// Opus mapping) to `W`.
///
/// Usage: [`OggOpusWriter::new`] immediately writes the two mandatory
/// header pages (`OpusHead` then `OpusTags`, each alone on its own page,
/// per RFC 7845 §3). Then call [`OggOpusWriter::write_packet`] once per
/// Opus packet, in order. Finally call [`OggOpusWriter::finish`] to flush
/// the last page with the EOS flag set and get the underlying writer back.
#[derive(Debug)]
pub struct OggOpusWriter<W: Write> {
    writer: W,
    serial: u32,
    page_sequence: u32,
    /// Cumulative granule position (samples at the 48 kHz Opus clock) of
    /// every packet written so far.
    granule_position: u64,
    /// Lacing values for packets buffered for the page currently being
    /// built.
    pending_segments: Vec<u8>,
    /// Packet bytes buffered for the page currently being built.
    pending_body: Vec<u8>,
    /// Samples (at 48 kHz) represented by `pending_body`, for the
    /// time-based flush heuristic.
    pending_samples: u64,
    finished: bool,
}

impl<W: Write> OggOpusWriter<W> {
    /// Creates a new writer and immediately emits the `OpusHead` (BOS) page
    /// and the `OpusTags` page.
    ///
    /// # Errors
    /// Returns an error if writing either header page fails.
    pub fn new(writer: W, serial: u32, params: OpusHeadParams) -> Result<Self> {
        let mut w = OggOpusWriter {
            writer,
            serial,
            page_sequence: 0,
            granule_position: 0,
            pending_segments: Vec::new(),
            pending_body: Vec::new(),
            pending_samples: 0,
            finished: false,
        };

        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(params.channels);
        head.extend_from_slice(&params.pre_skip.to_le_bytes());
        head.extend_from_slice(&params.input_sample_rate.to_le_bytes());
        head.extend_from_slice(&params.output_gain.to_le_bytes());
        head.push(0); // channel mapping family 0: single stream, no mapping table
        w.emit_page(&lacing_values(head.len()), &head, 0, true, false)
            .context("writing OpusHead page")?;

        let mut tags = Vec::with_capacity(8 + 4 + VENDOR.len() + 4);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(u32::try_from(VENDOR.len()).unwrap_or(u32::MAX)).to_le_bytes());
        tags.extend_from_slice(VENDOR);
        tags.extend_from_slice(&0u32.to_le_bytes()); // 0 user comments
        w.emit_page(&lacing_values(tags.len()), &tags, 0, false, false)
            .context("writing OpusTags page")?;

        Ok(w)
    }

    /// Buffers one Opus packet, computing its duration from its TOC byte
    /// (via [`opus_packet_duration_48k`]) to advance the granule position,
    /// and flushes the current page first if the packet wouldn't fit in
    /// its remaining lacing budget, or once the page holds about a second
    /// of audio.
    ///
    /// # Errors
    /// Returns an error if `packet` is empty or has an unparseable TOC
    /// byte (see [`opus_packet_duration_48k`]), if writing a flushed page
    /// fails, or if called after [`OggOpusWriter::finish`].
    pub fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        if self.finished {
            bail!("write_packet called after finish()");
        }
        let duration = opus_packet_duration_48k(packet).with_context(|| {
            format!("packet has an invalid Opus TOC byte (len={})", packet.len())
        })?;

        let needed = lacing_values(packet.len());
        if self.pending_segments.len() + needed.len() > MAX_PAGE_SEGMENTS {
            self.flush_page(false)?;
        }

        self.pending_segments.extend_from_slice(&needed);
        self.pending_body.extend_from_slice(packet);
        self.granule_position = self.granule_position.wrapping_add(u64::from(duration));
        self.pending_samples += u64::from(duration);

        if self.pending_samples >= MAX_PAGE_SAMPLES
            || self.pending_segments.len() >= MAX_PAGE_SEGMENTS - 4
        {
            self.flush_page(false)?;
        }
        Ok(())
    }

    /// Flushes any buffered packets as a final page with the EOS flag set,
    /// and returns the underlying writer.
    ///
    /// # Errors
    /// Returns an error if writing the final page fails.
    pub fn finish(mut self) -> Result<W> {
        self.finished = true;
        let segments = std::mem::take(&mut self.pending_segments);
        let body = std::mem::take(&mut self.pending_body);
        let granule = self.granule_position;
        self.emit_page(&segments, &body, granule, false, true)
            .context("writing final (EOS) page")?;
        Ok(self.writer)
    }

    fn flush_page(&mut self, eos: bool) -> Result<()> {
        if self.pending_segments.is_empty() && !eos {
            return Ok(());
        }
        let segments = std::mem::take(&mut self.pending_segments);
        let body = std::mem::take(&mut self.pending_body);
        let granule = self.granule_position;
        self.pending_samples = 0;
        self.emit_page(&segments, &body, granule, false, eos)
    }

    fn emit_page(
        &mut self,
        segments: &[u8],
        body: &[u8],
        granule: u64,
        bos: bool,
        eos: bool,
    ) -> Result<()> {
        debug_assert!(segments.len() <= MAX_PAGE_SEGMENTS);
        let page_segments = u8::try_from(segments.len()).unwrap_or(u8::MAX);

        let mut page = Vec::with_capacity(27 + segments.len() + body.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // stream structure version
        let mut header_type = 0u8;
        if bos {
            header_type |= 0x02;
        }
        if eos {
            header_type |= 0x04;
        }
        page.push(header_type);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.page_sequence.to_le_bytes());
        let crc_pos = page.len();
        page.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder, filled below
        page.push(page_segments);
        page.extend_from_slice(segments);
        page.extend_from_slice(body);

        let crc = ogg_crc32(&page);
        page[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

        self.writer.write_all(&page).context("writing Ogg page")?;
        self.page_sequence += 1;
        Ok(())
    }
}

/// Splits a packet length into Ogg lacing values (RFC 3533 §6): as many
/// 255s as needed, followed by exactly one value less than 255 (which may
/// be 0) to mark the packet's end. This is why an exact multiple of 255
/// bytes needs a trailing 0 entry rather than stopping at the last 255.
fn lacing_values(mut len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len / 255 + 1);
    while len >= 255 {
        out.push(255);
        len -= 255;
    }
    out.push(u8::try_from(len).unwrap_or(254));
    out
}

/// Per-config frame duration, in samples at the 48 kHz Opus clock,
/// indexed by the 5-bit `config` field of the TOC byte (RFC 6716 §3.1,
/// Table 2). Configs 0-11 are SILK-only (NB/MB/WB × 10/20/40/60 ms), 12-15
/// are Hybrid (SWB/FB × 10/20 ms), 16-31 are CELT-only (NB/WB/SWB/FB ×
/// 2.5/5/10/20 ms).
#[rustfmt::skip]
const CONFIG_FRAME_SAMPLES_48K: [u32; 32] = [
    // SILK NB
    480, 960, 1920, 2880,
    // SILK MB
    480, 960, 1920, 2880,
    // SILK WB
    480, 960, 1920, 2880,
    // Hybrid SWB
    480, 960,
    // Hybrid FB
    480, 960,
    // CELT NB
    120, 240, 480, 960,
    // CELT WB
    120, 240, 480, 960,
    // CELT SWB
    120, 240, 480, 960,
    // CELT FB
    120, 240, 480, 960,
];

/// Computes an Opus packet's duration, in samples at the 48 kHz Opus RTP
/// clock, from its TOC (table-of-contents) byte per RFC 6716 §3.1.
///
/// Returns `None` if `packet` is empty, or (for the "code 3", arbitrary
/// frame count case) if the frame-count byte is missing or declares 0
/// frames. This does not otherwise validate that `packet`'s length is
/// consistent with the declared frame count/sizes — it only extracts
/// enough to compute a duration, which is all an Ogg muxer needs to
/// advance the granule position.
#[must_use]
pub fn opus_packet_duration_48k(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = usize::from(toc >> 3);
    let code = toc & 0x03;
    let frame_samples = CONFIG_FRAME_SAMPLES_48K[config];

    let frame_count: u32 = match code {
        0 => 1,
        1 | 2 => 2,
        3 => {
            let fc_byte = *packet.get(1)?;
            let count = u32::from(fc_byte & 0x3f);
            if count == 0 {
                return None;
            }
            count
        }
        _ => unreachable!("code is toc & 0x03, always in 0..=3"),
    };

    Some(frame_samples * frame_count)
}

// --- Ogg CRC-32: polynomial 0x04c11db7, no reflection, init 0, no xorout. ---

#[allow(clippy::cast_possible_truncation)] // i < 256 always, so this never truncates
const fn build_crc_table() -> [u32; 256] {
    let poly: u32 = 0x04c1_1db7;
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            if r & 0x8000_0000 != 0 {
                r = (r << 1) ^ poly;
            } else {
                r <<= 1;
            }
            bit += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}

const CRC_TABLE: [u32; 256] = build_crc_table();

/// Ogg's page CRC (RFC 3533 §6): polynomial 0x04c11db7, computed
/// most-significant-byte first, with the page's own CRC field zeroed
/// during the computation, no input/output reflection, and no final XOR.
#[must_use]
pub fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in data {
        let idx = usize::from(u8::try_from(crc >> 24).unwrap_or(0) ^ b);
        crc = (crc << 8) ^ CRC_TABLE[idx];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io::Cursor;

    // ---------------------------------------------------------------
    // Test-only Ogg page reader, used to verify our own writer's output
    // (and, in the opusinfo-based integration test, to extract real Opus
    // packets out of an opusenc-generated file).
    // ---------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct ParsedPage {
        header_type: u8,
        granule: u64,
        serial: u32,
        sequence: u32,
        segments: Vec<u8>,
        body: Vec<u8>,
    }

    impl ParsedPage {
        fn bos(&self) -> bool {
            self.header_type & 0x02 != 0
        }
        fn eos(&self) -> bool {
            self.header_type & 0x04 != 0
        }
    }

    /// Parses every Ogg page in `data`, verifying the capture pattern and
    /// CRC of each one. Panics (via `expect`/`assert`) on malformed input:
    /// this is test-only code that only ever runs on our own writer's
    /// output or a real `opusenc` file, not on untrusted input.
    fn read_ogg_pages(mut data: &[u8]) -> Vec<ParsedPage> {
        let mut pages = Vec::new();
        while !data.is_empty() {
            assert!(data.len() >= 27, "page header truncated");
            assert_eq!(&data[0..4], b"OggS", "bad Ogg capture pattern");
            let header_type = data[5];
            let granule = u64::from_le_bytes(data[6..14].try_into().unwrap());
            let serial = u32::from_le_bytes(data[14..18].try_into().unwrap());
            let sequence = u32::from_le_bytes(data[18..22].try_into().unwrap());
            let crc_stored = u32::from_le_bytes(data[22..26].try_into().unwrap());
            let page_segments = usize::from(data[26]);
            assert!(data.len() >= 27 + page_segments, "segment table truncated");
            let segments = data[27..27 + page_segments].to_vec();
            let body_len: usize = segments.iter().map(|&s| usize::from(s)).sum();
            let total_len = 27 + page_segments + body_len;
            assert!(data.len() >= total_len, "page body truncated");
            let body = data[27 + page_segments..total_len].to_vec();

            let mut check = data[..total_len].to_vec();
            check[22..26].fill(0);
            let computed = ogg_crc32(&check);
            assert_eq!(computed, crc_stored, "CRC mismatch in Ogg page");

            pages.push(ParsedPage {
                header_type,
                granule,
                serial,
                sequence,
                segments,
                body,
            });
            data = &data[total_len..];
        }
        pages
    }

    /// Reassembles packets from a page list, honoring lacing (a 255
    /// segment continues the packet; anything less ends it), including
    /// continuation across page boundaries.
    fn reassemble_packets(pages: &[ParsedPage]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut current = Vec::new();
        for page in pages {
            let mut offset = 0usize;
            for &seg in &page.segments {
                let seg = usize::from(seg);
                current.extend_from_slice(&page.body[offset..offset + seg]);
                offset += seg;
                if seg < 255 {
                    packets.push(std::mem::take(&mut current));
                }
            }
        }
        packets
    }

    fn make_writer(serial: u32) -> OggOpusWriter<Cursor<Vec<u8>>> {
        OggOpusWriter::new(
            Cursor::new(Vec::new()),
            serial,
            OpusHeadParams {
                channels: 1,
                pre_skip: 312,
                input_sample_rate: 48_000,
                output_gain: 0,
            },
        )
        .unwrap()
    }

    /// A minimal but valid-TOC 20 ms mono Opus "packet" (config 1: SILK
    /// NB, code 0: one frame). Payload bytes beyond the TOC are not
    /// real encoded audio, only a plausible length, which is all our
    /// writer/CRC/lacing logic cares about.
    fn synth_packet(len: usize) -> Vec<u8> {
        let mut p = vec![0xAAu8; len.max(1)];
        p[0] = 0x08; // config=1 (SILK NB 20ms), stereo=0, code=0
        p
    }

    // --- CRC known-answer vectors ---

    #[test]
    fn crc_of_empty_is_zero() {
        assert_eq!(ogg_crc32(&[]), 0);
    }

    #[test]
    fn crc_of_single_zero_byte_is_zero() {
        assert_eq!(ogg_crc32(&[0x00]), 0);
    }

    #[test]
    fn crc_known_answer_single_byte() {
        // Hand-derived from the algorithm's own definition (poly
        // 0x04c11db7, MSB-first, init 0): CRC(0x01) == the polynomial
        // itself. See the module docs for the byte-by-byte derivation.
        assert_eq!(ogg_crc32(&[0x01]), 0x04c1_1db7);
    }

    #[test]
    fn crc_matches_bitwise_reference_implementation() {
        // Independent, non-table-driven implementation of the same
        // algorithm, cross-checked against the table-driven one.
        fn bitwise_crc(data: &[u8]) -> u32 {
            let poly: u32 = 0x04c1_1db7;
            let mut crc: u32 = 0;
            for &byte in data {
                crc ^= u32::from(byte) << 24;
                for _ in 0..8 {
                    if crc & 0x8000_0000 != 0 {
                        crc = (crc << 1) ^ poly;
                    } else {
                        crc <<= 1;
                    }
                }
            }
            crc
        }
        for sample in [
            &b""[..],
            &b"\x00"[..],
            &b"\x01"[..],
            &b"OggS"[..],
            &b"the quick brown fox jumps over the lazy dog"[..],
            &[0xffu8; 300][..],
        ] {
            assert_eq!(
                ogg_crc32(sample),
                bitwise_crc(sample),
                "mismatch for {sample:?}"
            );
        }
    }

    proptest! {
        #[test]
        fn crc_never_panics(data in prop::collection::vec(any::<u8>(), 0..2000)) {
            let _ = ogg_crc32(&data);
        }
    }

    // --- opus_packet_duration_48k / TOC table ---

    #[test]
    fn duration_table_spot_checks() {
        // (config, expected samples for a 1-frame/code-0 packet)
        let cases: &[(u8, u32)] = &[
            (0, 480),   // SILK NB 10ms
            (1, 960),   // SILK NB 20ms
            (2, 1920),  // SILK NB 40ms
            (3, 2880),  // SILK NB 60ms
            (8, 480),   // SILK WB 10ms
            (11, 2880), // SILK WB 60ms
            (12, 480),  // Hybrid SWB 10ms
            (13, 960),  // Hybrid SWB 20ms
            (14, 480),  // Hybrid FB 10ms
            (15, 960),  // Hybrid FB 20ms
            (16, 120),  // CELT NB 2.5ms
            (17, 240),  // CELT NB 5ms
            (18, 480),  // CELT NB 10ms
            (19, 960),  // CELT NB 20ms
            (28, 120),  // CELT FB 2.5ms
            (31, 960),  // CELT FB 20ms
        ];
        for &(config, expected) in cases {
            let toc = config << 3; // stereo=0, code=0
            assert_eq!(
                opus_packet_duration_48k(&[toc, 0]),
                Some(expected),
                "config {config}"
            );
        }
    }

    #[test]
    fn duration_code0_single_frame() {
        let toc: u8 = 1 << 3; // config 1, code 0
        assert_eq!(opus_packet_duration_48k(&[toc, 0, 0, 0]), Some(960));
    }

    #[test]
    fn duration_code1_two_equal_frames() {
        let toc = (1 << 3) | 1; // config 1, code 1
        assert_eq!(opus_packet_duration_48k(&[toc, 0, 0, 0]), Some(1920));
    }

    #[test]
    fn duration_code2_two_different_frames() {
        let toc = (1 << 3) | 2; // config 1, code 2
        assert_eq!(opus_packet_duration_48k(&[toc, 0, 0, 0]), Some(1920));
    }

    #[test]
    fn duration_code3_arbitrary_frame_count() {
        let toc = (16 << 3) | 3; // CELT NB 2.5ms, code 3
        let fc_byte = 5; // 5 frames, no VBR/padding bits
        assert_eq!(opus_packet_duration_48k(&[toc, fc_byte]), Some(120 * 5));
    }

    #[test]
    fn duration_code3_zero_frames_is_invalid() {
        let toc = (16 << 3) | 3;
        assert_eq!(opus_packet_duration_48k(&[toc, 0]), None);
    }

    #[test]
    fn duration_code3_missing_count_byte_is_invalid() {
        let toc = (16 << 3) | 3;
        assert_eq!(opus_packet_duration_48k(&[toc]), None);
    }

    #[test]
    fn duration_empty_packet_is_invalid() {
        assert_eq!(opus_packet_duration_48k(&[]), None);
    }

    #[test]
    fn duration_ignores_vbr_and_padding_bits_in_code3() {
        let toc = (16 << 3) | 3;
        let fc_byte = 0b1100_0010; // padding=1, vbr=1, count=2 -- count is bits 0-5
        // With padding bit set, a real parser would need to read more
        // padding-length bytes; our helper only needs the frame count for
        // duration and doesn't claim to fully parse code-3 padding.
        assert_eq!(opus_packet_duration_48k(&[toc, fc_byte]), Some(120 * 2));
    }

    proptest! {
        #[test]
        fn duration_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..8)) {
            let _ = opus_packet_duration_48k(&bytes);
        }
    }

    // --- lacing edge cases ---

    #[test]
    fn lacing_zero_bytes() {
        assert_eq!(lacing_values(0), vec![0]);
    }

    #[test]
    fn lacing_254_bytes() {
        assert_eq!(lacing_values(254), vec![254]);
    }

    #[test]
    fn lacing_255_bytes_needs_trailing_zero() {
        assert_eq!(lacing_values(255), vec![255, 0]);
    }

    #[test]
    fn lacing_510_bytes() {
        assert_eq!(lacing_values(510), vec![255, 255, 0]);
    }

    #[test]
    fn lacing_509_bytes() {
        assert_eq!(lacing_values(509), vec![255, 254]);
    }

    proptest! {
        #[test]
        fn lacing_sums_to_length(len in 0usize..5000) {
            let values = lacing_values(len);
            let sum: usize = values.iter().map(|&v| usize::from(v)).sum();
            prop_assert_eq!(sum, len);
            // Every value but possibly the last is 255; the last is < 255.
            if let Some((&last, rest)) = values.split_last() {
                prop_assert!(last < 255);
                prop_assert!(rest.iter().all(|&v| v == 255));
            }
        }
    }

    // --- page/stream-level structure ---

    #[test]
    fn header_pages_are_well_formed() {
        let w = make_writer(42);
        let buf = w.finish().unwrap().into_inner();
        let pages = read_ogg_pages(&buf);
        assert_eq!(pages.len(), 3); // OpusHead, OpusTags, empty EOS page
        assert!(pages[0].bos());
        assert!(!pages[0].eos());
        assert_eq!(&pages[0].body[0..8], b"OpusHead");
        assert!(!pages[1].bos());
        assert_eq!(&pages[1].body[0..8], b"OpusTags");
        assert!(pages[2].eos());
        for p in &pages {
            assert_eq!(p.serial, 42);
        }
    }

    #[test]
    fn granule_after_n_20ms_packets() {
        let mut w = make_writer(1);
        let packet = synth_packet(10); // config 1 = 20ms
        for _ in 0..25 {
            w.write_packet(&packet).unwrap();
        }
        let buf = w.finish().unwrap().into_inner();
        let pages = read_ogg_pages(&buf);
        let last_granule = pages.last().unwrap().granule;
        assert_eq!(last_granule, 25 * 960);
    }

    #[test]
    fn packets_round_trip_through_writer_and_reader() {
        let mut w = make_writer(7);
        let packets: Vec<Vec<u8>> = (1..=40).map(synth_packet).collect();
        for p in &packets {
            w.write_packet(p).unwrap();
        }
        let buf = w.finish().unwrap().into_inner();
        let pages = read_ogg_pages(&buf);

        // BOS/EOS flags: exactly one BOS (first page), exactly one EOS
        // (last page).
        assert!(pages[0].bos());
        assert_eq!(pages.iter().filter(|p| p.bos()).count(), 1);
        assert_eq!(pages.iter().filter(|p| p.eos()).count(), 1);
        assert!(pages.last().unwrap().eos());

        // Granule positions are monotonically non-decreasing (the two
        // header pages both carry granule 0).
        let mut prev = 0u64;
        for p in &pages {
            assert!(p.granule >= prev, "granule went backwards");
            prev = p.granule;
        }

        // Page sequence numbers are strictly increasing from 0.
        for (i, p) in pages.iter().enumerate() {
            assert_eq!(p.sequence, u32::try_from(i).unwrap());
        }

        // Reassembled packets: skip the OpusHead/OpusTags "packets" (the
        // header pages' bodies are each a single packet too).
        let mut reassembled = reassemble_packets(&pages);
        let opus_head = reassembled.remove(0);
        let opus_tags = reassembled.remove(0);
        assert_eq!(&opus_head[0..8], b"OpusHead");
        assert_eq!(&opus_tags[0..8], b"OpusTags");
        assert_eq!(reassembled, packets);
    }

    #[test]
    fn packet_needing_many_segments_does_not_panic_and_round_trips() {
        // 510-byte packet needs 3 lacing segments (255, 255, 0).
        let mut w = make_writer(3);
        let packet = synth_packet(510);
        w.write_packet(&packet).unwrap();
        let buf = w.finish().unwrap().into_inner();
        let pages = read_ogg_pages(&buf);
        let mut reassembled = reassemble_packets(&pages);
        reassembled.remove(0); // OpusHead
        reassembled.remove(0); // OpusTags
        assert_eq!(reassembled, vec![packet]);
    }

    #[test]
    fn write_packet_rejects_invalid_toc() {
        let mut w = make_writer(1);
        let err = w.write_packet(&[]).unwrap_err();
        assert!(err.to_string().contains("invalid Opus TOC"));
    }

    #[test]
    fn write_packet_after_finish_is_rejected() {
        let mut w = make_writer(1);
        w.write_packet(&synth_packet(10)).unwrap();
        // finish() consumes self, so simulate the "after finish" case by
        // manually setting the flag (finish() itself is the only public
        // way to reach it, exercised in other tests via move semantics).
        w.finished = true;
        let err = w.write_packet(&synth_packet(10)).unwrap_err();
        assert!(err.to_string().contains("after finish"));
    }

    proptest! {
        #[test]
        fn writer_never_panics_on_arbitrary_valid_length_packets(
            lens in prop::collection::vec(1usize..2000, 0..50),
        ) {
            let mut w = make_writer(9);
            for len in lens {
                let _ = w.write_packet(&synth_packet(len));
            }
            let _ = w.finish();
        }
    }

    // --- Real-world validation with opusinfo (opus-tools) ---

    /// End-to-end check: extract real Opus packets from a file produced by
    /// `opusenc`, re-mux them with our writer, and confirm `opusinfo`
    /// accepts the result. Skips at runtime (rather than failing) if
    /// `opusenc`/`opusinfo` aren't on PATH, since they're an external
    /// system dependency (`opus-tools`) not vendored into the repo.
    #[test]
    fn opusinfo_validates_remuxed_real_opus_file() {
        use std::process::Command;

        let have = |bin: &str| {
            Command::new(bin)
                .arg("--help")
                .output()
                .is_ok_and(|o| o.status.code().is_some())
        };
        if !have("opusenc") || !have("opusinfo") {
            eprintln!(
                "skipping opusinfo_validates_remuxed_real_opus_file: opus-tools not installed"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let wav_path = dir.path().join("tone.wav");
        let opus_path = dir.path().join("tone.opus");
        let remuxed_path = dir.path().join("remuxed.opus");

        write_test_tone_wav(&wav_path, 48_000, 1);

        let status = Command::new("opusenc")
            .args(["--quiet", "--bitrate", "24"])
            .arg(&wav_path)
            .arg(&opus_path)
            .status()
            .expect("failed to run opusenc");
        assert!(status.success(), "opusenc failed");

        let encoded = std::fs::read(&opus_path).unwrap();
        let pages = read_ogg_pages(&encoded);
        assert!(
            pages.len() >= 3,
            "expected at least header pages + one audio page"
        );
        let mut packets = reassemble_packets(&pages);
        // Drop OpusHead/OpusTags: everything else is real encoded audio.
        packets.remove(0);
        packets.remove(0);
        assert!(!packets.is_empty(), "opusenc produced no audio packets");

        let mut writer = OggOpusWriter::new(
            std::fs::File::create(&remuxed_path).unwrap(),
            0x5350_4B45, // arbitrary but stable serial ("SPKE")
            OpusHeadParams {
                channels: 1,
                pre_skip: 312,
                input_sample_rate: 48_000,
                output_gain: 0,
            },
        )
        .unwrap();
        for p in &packets {
            writer.write_packet(p).unwrap();
        }
        writer.finish().unwrap().flush().unwrap();

        let output = Command::new("opusinfo")
            .arg(&remuxed_path)
            .output()
            .expect("failed to run opusinfo");
        assert!(
            output.status.success(),
            "opusinfo rejected remuxed file:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Writes a minimal 16-bit PCM mono WAV file containing a short sine
    /// wave, with no external dependency (no `sox`): just enough for
    /// `opusenc` to accept as input.
    #[allow(clippy::cast_possible_truncation)] // sample is a scaled sine wave, always in i16 range
    fn write_test_tone_wav(path: &std::path::Path, sample_rate: u32, seconds: u32) {
        let num_samples = sample_rate * seconds;
        let mut data = Vec::with_capacity(num_samples as usize * 2);
        for n in 0..num_samples {
            let t = f64::from(n) / f64::from(sample_rate);
            let sample = (t * 440.0 * std::f64::consts::TAU).sin() * 0.25 * f64::from(i16::MAX);
            data.extend_from_slice(&(sample as i16).to_le_bytes());
        }

        let byte_rate = sample_rate * 2;
        let block_align: u16 = 2;
        let bits_per_sample: u16 = 16;
        let data_len = u32::try_from(data.len()).unwrap();

        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&data);

        std::fs::write(path, wav).unwrap();
    }
}
