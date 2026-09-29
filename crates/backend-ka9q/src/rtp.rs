// SPDX-License-Identifier: GPL-3.0-only
//! RTP header parsing (RFC 3550) and per-SSRC sequence/timestamp tracking for
//! radiod's Opus output.
//!
//! # radiod-specific facts (checked against the pinned upstream clone)
//!
//! - **Payload type.** radiod hard-codes payload type 111 for Opus
//!   (`OPUS_PT` in `src/rtp.c`, used by `src/opusd.c` and `src/opussend.c`);
//!   the comment there admits it "should be dynamic with sdp". We do **not**
//!   validate `payload_type` in [`RtpPacket::parse`] against a fixed value:
//!   the caller learns the encoding from the status protocol
//!   (`OUTPUT_ENCODING` / `enum encoding` in `rtp.h`, `OPUS = 3`) and decides
//!   whether to treat a packet as Opus.
//! - **Extension / CSRC / padding.** Every sender path in the pinned clone
//!   (`opusd.c`, `opussend.c`, `stereod.c`, `rdsd.c`, `pcmsend.c`,
//!   `packetd.c`, `audio.c`, ...) builds its `struct rtp_header` with
//!   `cc = 0` and never sets `extension`; several explicitly set
//!   `rtp.pad = 0` (e.g. `opusd.c:330`). radiod's own `ntoh_rtp`/`hton_rtp`
//!   (`src/rtp.c`) nonetheless support all three generically (extension
//!   words are skipped by length, CSRC identifiers are skipped by count), so
//!   in practice we will only ever see plain 12-byte headers from radiod,
//!   but [`RtpPacket::parse`] implements the general RFC 3550 framing rather
//!   than assuming that, since we're also expected to be robust to
//!   arbitrary/adversarial UDP input per the quality bar.
//! - Opus in radiod always uses the fixed 48 kHz RTP clock
//!   (`OPUS_SAMPRATE` in `rtp.h`), matching our `AudioFrame.timestamp`
//!   semantics (station-protocol.md §6.1).
//! - radiod's own sequence/duplicate/drop bookkeeping (`rtp_process` in
//!   `src/rtp.c`) treats the *signed 16-bit* difference between the
//!   incoming and expected sequence number as the step: negative means a
//!   duplicate/late packet, positive means one or more drops. We mirror
//!   that policy in [`SequenceTracker`] (see there for exact semantics),
//!   since it is the same "the sender's clock is authoritative" model
//!   RFC 3550 assumes and it works uniformly across the u16 wrap.

use std::fmt;

/// Errors returned while parsing an RTP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RtpError {
    /// The buffer is shorter than the fixed 12-byte RTP header.
    #[error("RTP packet too short: {len} bytes, need at least 12")]
    TooShort {
        /// Length of the input buffer, in bytes.
        len: usize,
    },
    /// The version field was not 2, the only version RFC 3550 (and radiod)
    /// uses.
    #[error("unsupported RTP version {0}")]
    UnsupportedVersion(u8),
    /// The CSRC list, header extension or padding claimed a length that ran
    /// past the end of the buffer.
    #[error("RTP header/extension/padding truncated")]
    Truncated,
    /// The padding byte (last byte of the packet, per RFC 3550 §5.1) is
    /// either 0 (illegal: padding must remove at least one byte) or larger
    /// than the number of bytes remaining after the fixed header, CSRC list
    /// and extension.
    #[error("invalid RTP padding length {0}")]
    InvalidPadding(u8),
}

/// A parsed RTP packet (RFC 3550 §5.1), borrowing its payload from the input
/// buffer.
///
/// `parse` is bounded and never panics: it is meant to run directly on bytes
/// read off a UDP socket, which are untrusted input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    /// RTP version. Always 2 after a successful parse.
    pub version: u8,
    /// Marker bit (M), profile-defined (radiod does not document a use for
    /// it on Opus channels).
    pub marker: bool,
    /// Payload type (PT), 7 bits. radiod uses 111 for Opus but this is not
    /// validated here; see the module docs.
    pub payload_type: u8,
    /// Sequence number, wraps at 2^16. Use [`SequenceTracker`] to turn this
    /// into a monotonically increasing station sequence number.
    pub seq: u16,
    /// RTP timestamp, in the stream's clock rate (48 kHz for Opus). Wraps at
    /// 2^32.
    pub timestamp: u32,
    /// Synchronization source identifier. radiod uses this as the channel
    /// (SSRC) identifier — it is the `OUTPUT_SSRC` the client chose when
    /// opening the channel.
    pub ssrc: u32,
    /// Contributing source identifiers, if any (radiod never sends any).
    pub csrc: Vec<u32>,
    /// Packet payload, with any header extension consumed and any trailing
    /// padding already stripped.
    pub payload: &'a [u8],
}

impl<'a> RtpPacket<'a> {
    /// Parses an RTP packet from a UDP payload.
    ///
    /// Handles the full RFC 3550 fixed header, the CSRC list, a (skipped,
    /// generic) header extension, and trailing padding. Never panics on
    /// arbitrary input; all length fields are checked against the remaining
    /// buffer before use.
    pub fn parse(buf: &'a [u8]) -> Result<Self, RtpError> {
        if buf.len() < 12 {
            return Err(RtpError::TooShort { len: buf.len() });
        }

        let b0 = buf[0];
        let version = b0 >> 6;
        if version != 2 {
            return Err(RtpError::UnsupportedVersion(version));
        }
        let has_padding = (b0 & 0b0010_0000) != 0;
        let has_extension = (b0 & 0b0001_0000) != 0;
        let cc = usize::from(b0 & 0x0f);

        let b1 = buf[1];
        let marker = (b1 & 0x80) != 0;
        let payload_type = b1 & 0x7f;

        let seq = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

        let mut pos = 12usize;

        // CSRC list: `cc` 32-bit identifiers.
        let csrc_bytes = cc * 4;
        let csrc_end = pos.checked_add(csrc_bytes).ok_or(RtpError::Truncated)?;
        if csrc_end > buf.len() {
            return Err(RtpError::Truncated);
        }
        let mut csrc = Vec::with_capacity(cc);
        for i in 0..cc {
            let off = pos + i * 4;
            csrc.push(u32::from_be_bytes([
                buf[off],
                buf[off + 1],
                buf[off + 2],
                buf[off + 3],
            ]));
        }
        pos = csrc_end;

        // Header extension (RFC 3550 §5.3.1): 16-bit profile-specific id,
        // then a 16-bit length in 32-bit words, then that many words. We
        // don't interpret the contents (radiod never sends one), only skip
        // it.
        if has_extension {
            if pos + 4 > buf.len() {
                return Err(RtpError::Truncated);
            }
            let ext_len_words = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
            let ext_header_and_body = 4usize
                .checked_add(ext_len_words.checked_mul(4).ok_or(RtpError::Truncated)?)
                .ok_or(RtpError::Truncated)?;
            let ext_end = pos
                .checked_add(ext_header_and_body)
                .ok_or(RtpError::Truncated)?;
            if ext_end > buf.len() {
                return Err(RtpError::Truncated);
            }
            pos = ext_end;
        }

        let mut payload_end = buf.len();
        if has_padding {
            // Last byte of the packet gives the padding length, including
            // itself (RFC 3550 §5.1).
            let pad_len_byte = *buf.last().ok_or(RtpError::Truncated)?;
            if pad_len_byte == 0 {
                return Err(RtpError::InvalidPadding(pad_len_byte));
            }
            let pad_len = usize::from(pad_len_byte);
            if pad_len > payload_end - pos {
                return Err(RtpError::InvalidPadding(pad_len_byte));
            }
            payload_end -= pad_len;
        }

        if pos > payload_end {
            return Err(RtpError::Truncated);
        }

        Ok(RtpPacket {
            version,
            marker,
            payload_type,
            seq,
            timestamp,
            ssrc,
            csrc,
            payload: &buf[pos..payload_end],
        })
    }
}

/// Why a packet was dropped by [`SequenceTracker::accept`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The packet's sequence number is not newer than the last one we
    /// accepted for this SSRC (a duplicate, or arrived out of order/late).
    /// Policy: drop it. We never reorder or buffer to wait for late
    /// packets — station-protocol's `AudioFrame`/waterfall paths are
    /// real-time and the hub tolerates gaps, not reordering.
    DuplicateOrLate,
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DropReason::DuplicateOrLate => {
                write!(f, "duplicate or late (out of order) RTP packet")
            }
        }
    }
}

/// Result of [`SequenceTracker::accept`] for a packet that was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    /// Our monotonically increasing station sequence number for this
    /// packet (`AudioFrame.seq` / `WaterfallRow.seq` semantics: increments
    /// by 1 per frame, wraps at 2^32).
    pub seq: u32,
    /// The RTP timestamp, re-based so the first accepted packet for this
    /// SSRC has timestamp 0 (`AudioFrame.timestamp` semantics: 48 kHz Opus
    /// clock, wraps at 2^32).
    pub timestamp: u32,
    /// Number of frames judged lost immediately before this one (gap in the
    /// RTP sequence number since the last accepted packet). 0 for the first
    /// packet of a stream and for consecutive packets.
    pub lost_before: u32,
}

/// Per-SSRC state that turns radiod's 16-bit RTP sequence numbers into a
/// monotonically increasing `u32` station sequence number, detects gaps
/// (lost packets), duplicates and reordering.
///
/// # Reordering / duplicate policy
///
/// RTP sequence numbers only ever *increase* modulo 2^16 in a well-behaved
/// stream; radiod's own `rtp_process` (`src/rtp.c`) uses the sign of the
/// 16-bit signed difference `seq - expected` to tell a genuine gap (positive
/// step: packets were lost) from a duplicate or late/reordered packet
/// (negative step). We use the same test. A late or duplicate packet is
/// **dropped**, not buffered or reinserted: stationd's job is to forward a
/// live stream, and reordering audio after the fact would need buffering
/// depth station-protocol's real-time model doesn't have. This does mean a
/// genuinely reordered (not lost) packet is counted as a drop rather than
/// recovered — an accepted tradeoff for a live relay.
///
/// `timestamp` re-basing: the first packet accepted for an SSRC defines
/// timestamp 0; every later timestamp is reported as
/// `rtp_timestamp.wrapping_sub(base_timestamp)`, which is well-defined
/// across the RTP timestamp's own u32 wraparound because we're using
/// wrapping arithmetic throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceTracker {
    ssrc: u32,
    initialized: bool,
    /// Last accepted RTP sequence number.
    last_seq: u16,
    /// RTP timestamp of the first accepted packet (subtracted from every
    /// later timestamp).
    base_timestamp: u32,
    /// Our own monotonically increasing output counter.
    next_out_seq: u32,
    /// Total packets seen (accepted or dropped), for diagnostics.
    pub packets: u64,
    /// Total packets judged lost (gaps), for diagnostics.
    pub lost: u64,
    /// Total packets dropped as duplicate/late, for diagnostics.
    pub dropped: u64,
}

impl SequenceTracker {
    /// Creates a tracker bound to a specific SSRC (channel). Packets for a
    /// different SSRC are treated as a fresh stream: like radiod's own
    /// `rtp_process`, we reset all state instead of rejecting them, since a
    /// station backend demuxes by SSRC upstream of this tracker and a
    /// changed SSRC on the same tracker would only happen if the caller
    /// reused it for a new channel.
    #[must_use]
    pub fn new(ssrc: u32) -> Self {
        SequenceTracker {
            ssrc,
            initialized: false,
            last_seq: 0,
            base_timestamp: 0,
            next_out_seq: 0,
            packets: 0,
            lost: 0,
            dropped: 0,
        }
    }

    /// The SSRC this tracker is currently bound to.
    #[must_use]
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Processes one packet's `(ssrc, seq, timestamp)`, updating internal
    /// state and returning either the accepted, station-facing sequence
    /// number/timestamp/loss-count, or the reason the packet was dropped.
    pub fn accept(&mut self, ssrc: u32, seq: u16, timestamp: u32) -> Result<Accepted, DropReason> {
        if ssrc != self.ssrc || !self.initialized {
            // New stream (or first packet ever): (re)synchronize, mirroring
            // radiod's own `rtp_process`.
            self.ssrc = ssrc;
            self.initialized = true;
            self.last_seq = seq;
            self.base_timestamp = timestamp;
            self.next_out_seq = 0;
            self.packets = 1;
            let out = Accepted {
                seq: self.next_out_seq,
                timestamp: 0,
                lost_before: 0,
            };
            self.next_out_seq = self.next_out_seq.wrapping_add(1);
            return Ok(out);
        }

        self.packets += 1;
        // Reinterpret the 16-bit modular difference as signed, exactly like
        // radiod's own `(int16_t)(rtp->seq - state->seq)` in `rtp_process`
        // (src/rtp.c): a positive step means packets were lost, zero/negative
        // means a duplicate or late/reordered packet.
        #[allow(clippy::cast_possible_wrap)]
        let step = seq.wrapping_sub(self.last_seq) as i16;
        if step <= 0 {
            // Duplicate (step == 0) or late/reordered (step < 0) packet.
            self.dropped += 1;
            return Err(DropReason::DuplicateOrLate);
        }

        // step > 0 here, so the sign-losing cast below is safe.
        #[allow(clippy::cast_sign_loss)]
        let step_u16 = step as u16;
        let lost_before = u32::from(step_u16 - 1);
        self.lost += u64::from(lost_before);
        self.last_seq = seq;

        let out = Accepted {
            seq: self.next_out_seq,
            timestamp: timestamp.wrapping_sub(self.base_timestamp),
            lost_before,
        };
        self.next_out_seq = self.next_out_seq.wrapping_add(1);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Builds a minimal 12-byte RTP header (no CSRC, no extension, no
    /// padding) followed by `payload`.
    #[allow(clippy::too_many_arguments)] // test-only builder, one arg per header field
    fn build_header(
        version: u8,
        pad: bool,
        ext: bool,
        cc: u8,
        marker: bool,
        pt: u8,
        seq: u16,
        ts: u32,
        ssrc: u32,
        csrc: &[u32],
        ext_words: &[u32],
        payload: &[u8],
        pad_len: u8,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let b0 = (version << 6) | (u8::from(pad) << 5) | (u8::from(ext) << 4) | (cc & 0x0f);
        buf.push(b0);
        let b1 = (u8::from(marker) << 7) | (pt & 0x7f);
        buf.push(b1);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&ts.to_be_bytes());
        buf.extend_from_slice(&ssrc.to_be_bytes());
        for c in csrc {
            buf.extend_from_slice(&c.to_be_bytes());
        }
        if ext {
            buf.extend_from_slice(&0xBEEFu16.to_be_bytes()); // profile-specific id
            let ext_len = u16::try_from(ext_words.len()).unwrap_or(u16::MAX);
            buf.extend_from_slice(&ext_len.to_be_bytes());
            for w in ext_words {
                buf.extend_from_slice(&w.to_be_bytes());
            }
        }
        buf.extend_from_slice(payload);
        if pad {
            let filler = usize::from(pad_len.saturating_sub(1));
            buf.resize(buf.len() + filler, 0);
            buf.push(pad_len);
        }
        buf
    }

    #[test]
    fn parses_minimal_header() {
        let buf = build_header(
            2,
            false,
            false,
            0,
            true,
            111,
            42,
            1000,
            0xdead_beef,
            &[],
            &[],
            b"opus-bytes",
            0,
        );
        let pkt = RtpPacket::parse(&buf).unwrap();
        assert_eq!(pkt.version, 2);
        assert!(pkt.marker);
        assert_eq!(pkt.payload_type, 111);
        assert_eq!(pkt.seq, 42);
        assert_eq!(pkt.timestamp, 1000);
        assert_eq!(pkt.ssrc, 0xdead_beef);
        assert!(pkt.csrc.is_empty());
        assert_eq!(pkt.payload, b"opus-bytes");
    }

    #[test]
    fn parses_with_csrc() {
        let csrc = [1u32, 2, 3];
        let buf = build_header(
            2,
            false,
            false,
            3,
            false,
            111,
            1,
            1,
            5,
            &csrc,
            &[],
            b"pl",
            0,
        );
        let pkt = RtpPacket::parse(&buf).unwrap();
        assert_eq!(pkt.csrc, vec![1, 2, 3]);
        assert_eq!(pkt.payload, b"pl");
    }

    #[test]
    fn parses_with_extension() {
        let ext_words = [0xAAAA_AAAAu32, 0xBBBB_BBBB];
        let buf = build_header(
            2,
            false,
            true,
            0,
            false,
            111,
            1,
            1,
            5,
            &[],
            &ext_words,
            b"data",
            0,
        );
        let pkt = RtpPacket::parse(&buf).unwrap();
        assert_eq!(pkt.payload, b"data");
    }

    #[test]
    fn parses_with_padding() {
        let buf = build_header(2, true, false, 0, false, 111, 1, 1, 5, &[], &[], b"data", 4);
        let pkt = RtpPacket::parse(&buf).unwrap();
        assert_eq!(pkt.payload, b"data");
    }

    #[test]
    fn parses_with_csrc_extension_and_padding_combined() {
        let csrc = [7u32];
        let ext_words = [0x1234_5678u32];
        let buf = build_header(
            2,
            true,
            true,
            1,
            true,
            111,
            999,
            5000,
            42,
            &csrc,
            &ext_words,
            b"hello world",
            3,
        );
        let pkt = RtpPacket::parse(&buf).unwrap();
        assert_eq!(pkt.csrc, vec![7]);
        assert_eq!(pkt.payload, b"hello world");
        assert_eq!(pkt.seq, 999);
        assert_eq!(pkt.timestamp, 5000);
        assert_eq!(pkt.ssrc, 42);
    }

    #[test]
    fn rejects_too_short() {
        let buf = [0u8; 11];
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::TooShort { len: 11 }));
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = build_header(2, false, false, 0, false, 111, 1, 1, 1, &[], &[], b"x", 0);
        buf[0] = (1 << 6) | (buf[0] & 0x3f); // version 1
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::UnsupportedVersion(1)));
    }

    #[test]
    fn rejects_version_3() {
        let mut buf = build_header(2, false, false, 0, false, 111, 1, 1, 1, &[], &[], b"x", 0);
        buf[0] = (3 << 6) | (buf[0] & 0x3f);
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::UnsupportedVersion(3)));
    }

    #[test]
    fn rejects_truncated_csrc() {
        // cc = 2 claims 8 bytes of CSRC but buffer only has the fixed header.
        let mut buf = vec![0u8; 12];
        buf[0] = (2 << 6) | 2; // version 2, cc = 2
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::Truncated));
    }

    #[test]
    fn rejects_truncated_extension() {
        let mut buf = vec![0u8; 12];
        buf[0] = (2 << 6) | 0b0001_0000; // version 2, extension bit set
        // No extension header bytes follow.
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::Truncated));
    }

    #[test]
    fn rejects_extension_length_past_end() {
        let mut buf = vec![0u8; 16];
        buf[0] = (2 << 6) | 0b0001_0000;
        buf[14] = 0xff; // ext_len_words huge
        buf[15] = 0xff;
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::Truncated));
    }

    #[test]
    fn rejects_zero_padding_length() {
        let mut buf = build_header(2, true, false, 0, false, 111, 1, 1, 1, &[], &[], b"x", 0);
        // build_header with pad_len 0 writes a single 0 byte as the pad-length byte.
        *buf.last_mut().unwrap() = 0;
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::InvalidPadding(0)));
    }

    #[test]
    fn rejects_padding_longer_than_payload() {
        let mut buf = vec![0u8; 12];
        buf[0] = (2 << 6) | 0b0010_0000; // padding bit set
        buf.push(200); // pad length far exceeds the 0-byte payload
        assert_eq!(RtpPacket::parse(&buf), Err(RtpError::InvalidPadding(200)));
    }

    #[test]
    fn never_panics_on_arbitrary_short_buffers() {
        // Exhaustively fuzz small buffers instead of relying only on proptest.
        for len in 0..40 {
            let mut buf = vec![0xffu8; len];
            for b0 in 0..=255u8 {
                if let Some(b) = buf.first_mut() {
                    *b = b0;
                }
                let _ = RtpPacket::parse(&buf);
            }
        }
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(buf in prop::collection::vec(any::<u8>(), 0..300)) {
            let _ = RtpPacket::parse(&buf);
        }

        #[test]
        fn round_trip_random_fields(
            marker in any::<bool>(),
            pt in 0u8..128,
            seq in any::<u16>(),
            ts in any::<u32>(),
            ssrc in any::<u32>(),
            csrc in prop::collection::vec(any::<u32>(), 0..15),
            payload in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            let cc = u8::try_from(csrc.len()).unwrap_or(u8::MAX);
            let buf = build_header(2, false, false, cc, marker, pt, seq, ts, ssrc, &csrc, &[], &payload, 0);
            let pkt = RtpPacket::parse(&buf).unwrap();
            prop_assert_eq!(pkt.marker, marker);
            prop_assert_eq!(pkt.payload_type, pt);
            prop_assert_eq!(pkt.seq, seq);
            prop_assert_eq!(pkt.timestamp, ts);
            prop_assert_eq!(pkt.ssrc, ssrc);
            prop_assert_eq!(pkt.csrc, csrc);
            prop_assert_eq!(pkt.payload, payload.as_slice());
        }

        #[test]
        fn round_trip_with_padding(
            payload in prop::collection::vec(any::<u8>(), 0..64),
            pad_len in 1u8..=30,
        ) {
            let buf = build_header(2, true, false, 0, false, 111, 1, 1, 1, &[], &[], &payload, pad_len);
            let pkt = RtpPacket::parse(&buf).unwrap();
            prop_assert_eq!(pkt.payload, payload.as_slice());
        }
    }

    // --- SequenceTracker ---

    #[test]
    fn tracker_first_packet_is_seq_zero_ts_zero() {
        let mut t = SequenceTracker::new(1);
        let a = t.accept(1, 5000, 123_456).unwrap();
        assert_eq!(
            a,
            Accepted {
                seq: 0,
                timestamp: 0,
                lost_before: 0
            }
        );
    }

    #[test]
    fn tracker_consecutive_packets_increment_by_one() {
        let mut t = SequenceTracker::new(1);
        t.accept(1, 100, 0).unwrap();
        let a = t.accept(1, 101, 960).unwrap();
        assert_eq!(
            a,
            Accepted {
                seq: 1,
                timestamp: 960,
                lost_before: 0
            }
        );
        let a = t.accept(1, 102, 1920).unwrap();
        assert_eq!(
            a,
            Accepted {
                seq: 2,
                timestamp: 1920,
                lost_before: 0
            }
        );
    }

    #[test]
    fn tracker_detects_gap() {
        let mut t = SequenceTracker::new(1);
        t.accept(1, 100, 0).unwrap();
        // Skip seq 101..104, arrive at 105: 4 lost.
        let a = t.accept(1, 105, 4800).unwrap();
        assert_eq!(
            a,
            Accepted {
                seq: 1,
                timestamp: 4800,
                lost_before: 4
            }
        );
        assert_eq!(t.lost, 4);
    }

    #[test]
    fn tracker_drops_duplicate() {
        let mut t = SequenceTracker::new(1);
        t.accept(1, 100, 0).unwrap();
        t.accept(1, 101, 960).unwrap();
        let err = t.accept(1, 101, 960).unwrap_err();
        assert_eq!(err, DropReason::DuplicateOrLate);
        assert_eq!(t.dropped, 1);
    }

    #[test]
    fn tracker_drops_late_reordered_packet() {
        let mut t = SequenceTracker::new(1);
        t.accept(1, 100, 0).unwrap();
        t.accept(1, 102, 1920).unwrap(); // gap of 1 (101 lost)
        let err = t.accept(1, 101, 960).unwrap_err(); // arrives late
        assert_eq!(err, DropReason::DuplicateOrLate);
    }

    #[test]
    fn tracker_seq_wraps_65535_to_0() {
        let mut t = SequenceTracker::new(1);
        t.accept(1, 65535, 0).unwrap();
        let a = t.accept(1, 0, 960).unwrap();
        assert_eq!(
            a,
            Accepted {
                seq: 1,
                timestamp: 960,
                lost_before: 0
            }
        );
    }

    #[test]
    fn tracker_out_seq_wraps_at_u32_max() {
        let mut t = SequenceTracker::new(1);
        t.next_out_seq = u32::MAX;
        t.initialized = true;
        t.last_seq = 0;
        t.base_timestamp = 0;
        let a = t.accept(1, 1, 100).unwrap();
        assert_eq!(a.seq, u32::MAX); // last pre-wrap value
        let a2 = t.accept(1, 2, 200).unwrap();
        assert_eq!(a2.seq, 0); // wrapped
    }

    #[test]
    fn tracker_timestamp_rebases_from_nonzero_base() {
        // Use a base that will wrap when subsequent timestamps advance past u32::MAX.
        let mut t = SequenceTracker::new(2);
        let base = u32::MAX - 100;
        t.accept(2, 10, base).unwrap();
        let a = t.accept(2, 11, base.wrapping_add(200)).unwrap();
        // 200 samples after a base near the top of u32 range should rebase to 200,
        // even though `base + 200` wrapped around u32::MAX.
        assert_eq!(a.timestamp, 200);
    }

    #[test]
    fn tracker_reinitializes_on_ssrc_change() {
        let mut t = SequenceTracker::new(1);
        t.accept(1, 100, 500).unwrap();
        let a = t.accept(2, 7, 9).unwrap();
        assert_eq!(
            a,
            Accepted {
                seq: 0,
                timestamp: 0,
                lost_before: 0
            }
        );
        assert_eq!(t.ssrc(), 2);
    }

    proptest! {
        #[test]
        fn tracker_never_panics(
            ssrc in any::<u32>(),
            events in prop::collection::vec((any::<u16>(), any::<u32>()), 0..200),
        ) {
            let mut t = SequenceTracker::new(ssrc);
            for (seq, ts) in events {
                let _ = t.accept(ssrc, seq, ts);
            }
        }

        #[test]
        fn tracker_out_seq_monotonic_for_accepted(
            start in any::<u16>(),
            steps in prop::collection::vec(1u16..500, 0..100),
        ) {
            let mut t = SequenceTracker::new(1);
            let mut seq = start;
            t.accept(1, seq, 0).unwrap();
            let mut last_out = 0u32;
            for step in steps {
                seq = seq.wrapping_add(step);
                let a = t.accept(1, seq, 0).unwrap();
                prop_assert_eq!(a.seq, last_out.wrapping_add(1));
                last_out = a.seq;
            }
        }
    }
}
