// SPDX-License-Identifier: GPL-3.0-only
//! TLV codec for radiod command and status packets.
//!
//! Faithful to upstream `status.c` at the pinned commit (see
//! [`crate::tags::UPSTREAM_COMMIT`]):
//!
//! - A packet is one [`PktType`] byte followed by TLVs and an `EOL` (0) tag,
//!   which has no length byte.
//! - Each TLV is `tag: u8`, a length, then the value. A length below 128 is
//!   one byte; otherwise it is `0x80 | n` followed by `n` big-endian length
//!   bytes (`n` at most 4).
//! - Integers are big-endian with leading zero bytes removed, so zero has
//!   length 0. Signed C `int`s are cast to `uint64_t` first, so negative
//!   values are sign-extended to 8 bytes ([`Encoder::i32`]).
//! - `float` is the `f32` bit pattern encoded as an integer and `double` the
//!   `f64` bit pattern; both use leading-zero suppression, so `0.0` has length
//!   0. NaN is never encoded (the field is omitted).
//! - Sockets are 6 bytes (IPv4 address, port) or 18 bytes (IPv6 address,
//!   port), in network byte order.
//!
//! # Decoder behaviour
//!
//! [`parse`] validates the whole TLV list up front and is bounded and
//! panic-free on arbitrary bytes:
//!
//! - Parsing stops at the first `EOL`; bytes after it are ignored (as in
//!   radiod).
//! - A missing `EOL` at the end of the buffer is tolerated.
//! - A trailing partial TLV (a tag without a length, missing length bytes,
//!   or a value running past the end) is an error for the whole packet. UDP
//!   delivers datagrams whole, so a partial TLV means a corrupt or foreign
//!   packet; we do not guess which fields before it are trustworthy.
//! - Unknown tags are kept as raw [`Field`]s so callers can skip them.
//!
//! radiod's own parser drops a TLV whose value ends exactly at the end of the
//! buffer (it requires `cp + optlen < end`), so packets sent to radiod must
//! always end with `EOL`; [`Encoder::finish`] always appends it.

use std::borrow::Cow;
use std::iter::FusedIterator;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use thiserror::Error;

use crate::tags::{PktType, StatusType};

/// Largest `n` accepted in the `0x80 | n` long-length form (upstream writes at
/// most 4 length bytes).
pub const MAX_LENGTH_OF_LENGTH: u8 = 4;

/// Errors from decoding TLV packets and field values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TlvError {
    /// The buffer is empty (no packet type byte).
    #[error("empty packet")]
    Empty,
    /// The first byte is not a known [`PktType`].
    #[error("unknown packet type {0}")]
    UnknownPacketType(u8),
    /// The packet is a valid TLV packet of the wrong type.
    #[error("expected a {expected} packet, got {got}")]
    WrongPacketType {
        /// The packet type the caller wanted.
        expected: PktType,
        /// The packet type found.
        got: PktType,
    },
    /// A TLV ends before its length field is complete.
    #[error("truncated TLV for tag {tag} at offset {offset}")]
    Truncated {
        /// Raw tag byte.
        tag: u8,
        /// Offset of the tag byte in the packet.
        offset: usize,
    },
    /// The long-length form uses more than [`MAX_LENGTH_OF_LENGTH`] bytes.
    #[error("TLV for tag {tag} at offset {offset} has a {n}-byte length field (max 4)")]
    LengthOfLength {
        /// Raw tag byte.
        tag: u8,
        /// Offset of the tag byte in the packet.
        offset: usize,
        /// The declared number of length bytes.
        n: u8,
    },
    /// The value length runs past the end of the buffer.
    #[error(
        "TLV for tag {tag} at offset {offset} has length {len} but only {remaining} bytes remain"
    )]
    LengthExceedsBuffer {
        /// Raw tag byte.
        tag: u8,
        /// Offset of the tag byte in the packet.
        offset: usize,
        /// The declared value length.
        len: usize,
        /// Bytes left in the buffer after the length field.
        remaining: usize,
    },
    /// An integer (or float/double bit pattern) value is longer than 8 bytes.
    #[error("integer value of {0} bytes is longer than 8")]
    IntegerTooLong(usize),
    /// A socket value is not 6 (IPv4) or 18 (IPv6) bytes.
    #[error("socket value must be 6 or 18 bytes, got {0}")]
    BadSocketLength(usize),
    /// A string value is not valid UTF-8 (see [`Field::as_str`]).
    #[error("string value is not valid UTF-8")]
    InvalidUtf8,
    /// A float array value is not a multiple of 4 bytes.
    #[error("float array of {0} bytes is not a multiple of 4")]
    BadFloatArrayLength(usize),
    /// A known field has a malformed value.
    #[error("invalid {tag} field: {source}")]
    Field {
        /// The field's tag.
        tag: StatusType,
        /// Why its value is malformed.
        #[source]
        source: Box<TlvError>,
    },
}

/// Builds a command or status packet. Every method appends one TLV; call
/// [`Encoder::finish`] to append `EOL` and take the bytes.
///
/// ```
/// use backend_ka9q::tags::{PktType, StatusType};
/// use backend_ka9q::tlv::Encoder;
///
/// let pkt = Encoder::new(PktType::Cmd)
///     .u32(StatusType::OutputSsrc, 0x1234)
///     .finish();
/// assert_eq!(pkt, [1, 18, 2, 0x12, 0x34, 0]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// Starts a packet of the given type.
    pub fn new(kind: PktType) -> Self {
        Self {
            buf: vec![kind.value()],
        }
    }

    /// Appends the length field (`encode_string` rules).
    fn put_len(&mut self, len: usize) {
        let Ok(len32) = u32::try_from(len) else {
            // Upstream silently truncates here; a >4 GiB value cannot be sent
            // in a UDP datagram anyway, so treat it as a caller bug.
            panic!("TLV value of {len} bytes exceeds the 4-byte length form");
        };
        let b = len32.to_be_bytes();
        if len32 < 128 {
            self.buf.push(b[3]);
        } else if len32 < 0x1_0000 {
            self.buf.extend_from_slice(&[0x82, b[2], b[3]]);
        } else if len32 < 0x100_0000 {
            self.buf.extend_from_slice(&[0x83, b[1], b[2], b[3]]);
        } else {
            self.buf.extend_from_slice(&[0x84, b[0], b[1], b[2], b[3]]);
        }
    }

    /// Unsigned integer, big-endian with leading zeros removed (`encode_int64`).
    pub fn u64(mut self, tag: StatusType, v: u64) -> Self {
        self.buf.push(tag.value());
        let bytes = v.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        let value = bytes.get(skip..).unwrap_or_default();
        self.put_len(value.len());
        self.buf.extend_from_slice(value);
        self
    }

    /// 32-bit unsigned integer (`encode_int32`).
    pub fn u32(self, tag: StatusType, v: u32) -> Self {
        self.u64(tag, u64::from(v))
    }

    /// 16-bit unsigned integer (`encode_int16`).
    pub fn u16(self, tag: StatusType, v: u16) -> Self {
        self.u64(tag, u64::from(v))
    }

    /// C `int` (`encode_int`), which casts the value to `uint64_t`. Negative
    /// values are therefore sign-extended and take 8 bytes: `-1` is encoded as
    /// eight `0xff` bytes. `decode_int` masks back to 32 bits, so
    /// [`Field::as_i32`] recovers the original value.
    pub fn i32(self, tag: StatusType, v: i32) -> Self {
        self.i64(tag, i64::from(v))
    }

    /// Signed 64-bit integer as radiod sends it (`int64_t` passed to
    /// `encode_int64`, e.g. `GPS_TIME`): two's complement bits.
    pub fn i64(self, tag: StatusType, v: i64) -> Self {
        self.u64(tag, u64::from_ne_bytes(v.to_ne_bytes()))
    }

    /// Boolean (`encode_bool`): `false` has length 0, `true` is one byte `1`.
    pub fn bool(self, tag: StatusType, v: bool) -> Self {
        self.byte(tag, u8::from(v))
    }

    /// One byte (`encode_byte`): zero has length 0.
    pub fn byte(self, tag: StatusType, v: u8) -> Self {
        self.u64(tag, u64::from(v))
    }

    /// C `float` (`encode_float`): the value is rounded to `f32` and its bit
    /// pattern encoded as an integer. NaN is skipped (nothing is appended).
    pub fn f32(self, tag: StatusType, v: f64) -> Self {
        if v.is_nan() {
            return self;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "C `(float)x` rounding is the wire format"
        )]
        let f = v as f32;
        self.u32(tag, f.to_bits())
    }

    /// C `double` (`encode_double`): the `f64` bit pattern encoded as an
    /// integer. NaN is skipped (nothing is appended).
    pub fn f64(self, tag: StatusType, v: f64) -> Self {
        if v.is_nan() {
            return self;
        }
        self.u64(tag, v.to_bits())
    }

    /// Raw bytes (`encode_string`), using the long-length form from 128
    /// bytes on.
    ///
    /// # Panics
    ///
    /// If `v` is 4 GiB or longer, which no UDP packet can carry.
    pub fn bytes(mut self, tag: StatusType, v: &[u8]) -> Self {
        self.buf.push(tag.value());
        self.put_len(v.len());
        self.buf.extend_from_slice(v);
        self
    }

    /// UTF-8 string, without a trailing NUL (`encode_string`).
    ///
    /// # Panics
    ///
    /// If `s` is 4 GiB or longer.
    pub fn str(self, tag: StatusType, s: &str) -> Self {
        self.bytes(tag, s.as_bytes())
    }

    /// Socket address (`encode_socket`): IPv4 address and port (6 bytes) or
    /// IPv6 address and port (18 bytes). IPv6 flow info and scope id are not
    /// carried.
    pub fn socket(mut self, tag: StatusType, addr: SocketAddr) -> Self {
        self.buf.push(tag.value());
        match addr {
            SocketAddr::V4(a) => {
                self.buf.push(6);
                self.buf.extend_from_slice(&a.ip().octets());
            }
            SocketAddr::V6(a) => {
                self.buf.push(18);
                self.buf.extend_from_slice(&a.ip().octets());
            }
        }
        self.buf.extend_from_slice(&addr.port().to_be_bytes());
        self
    }

    /// Vector of `f32`, each as 4 big-endian bytes without zero suppression
    /// (`encode_vector`, used for `BIN_DATA`).
    ///
    /// # Panics
    ///
    /// If the array is 4 GiB or longer.
    pub fn f32_array(mut self, tag: StatusType, v: &[f32]) -> Self {
        self.buf.push(tag.value());
        self.put_len(v.len().saturating_mul(4));
        for f in v {
            self.buf.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        self
    }

    /// Appends `EOL` and returns the packet bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push(StatusType::Eol.value());
        self.buf
    }
}

/// One TLV of a parsed packet. The tag is kept raw so unknown tags can be
/// skipped; the accessors mirror upstream `decode_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field<'a> {
    /// Raw tag byte (never `EOL`).
    pub tag: u8,
    /// The value bytes.
    pub value: &'a [u8],
}

impl<'a> Field<'a> {
    /// The tag as a known [`StatusType`], or `None` for a tag this build
    /// does not know.
    pub fn status_type(&self) -> Option<StatusType> {
        StatusType::from_u8(self.tag)
    }

    /// Unsigned integer (`decode_int64`). Values longer than 8 bytes are
    /// rejected (upstream would silently keep the low 8 bytes).
    pub fn as_u64(&self) -> Result<u64, TlvError> {
        if self.value.len() > 8 {
            return Err(TlvError::IntegerTooLong(self.value.len()));
        }
        Ok(self
            .value
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    /// Low 32 bits of the integer (`decode_int32` masks, it does not reject).
    #[expect(clippy::cast_possible_truncation, reason = "C masking semantics")]
    pub fn as_u32(&self) -> Result<u32, TlvError> {
        Ok(self.as_u64()? as u32)
    }

    /// Low 16 bits of the integer (`decode_int16`).
    #[expect(clippy::cast_possible_truncation, reason = "C masking semantics")]
    pub fn as_u16(&self) -> Result<u16, TlvError> {
        Ok(self.as_u64()? as u16)
    }

    /// Low 8 bits of the integer (`decode_int8`).
    #[expect(clippy::cast_possible_truncation, reason = "C masking semantics")]
    pub fn as_u8(&self) -> Result<u8, TlvError> {
        Ok(self.as_u64()? as u8)
    }

    /// Any non-zero integer is `true` (`decode_bool`).
    pub fn as_bool(&self) -> Result<bool, TlvError> {
        Ok(self.as_u64()? != 0)
    }

    /// C `int` (`decode_int`): the low 32 bits reinterpreted as signed, so
    /// the 8-byte sign-extended encoding of a negative `int` round-trips.
    pub fn as_i32(&self) -> Result<i32, TlvError> {
        Ok(i32::from_ne_bytes(self.as_u32()?.to_ne_bytes()))
    }

    /// Signed 64-bit integer (two's complement of `decode_int64`), for
    /// fields radiod sends from `int64_t` such as `GPS_TIME`.
    pub fn as_i64(&self) -> Result<i64, TlvError> {
        Ok(i64::from_ne_bytes(self.as_u64()?.to_ne_bytes()))
    }

    /// C `decode_float`: length 0 is `0.0`; up to 4 bytes are an `f32` bit
    /// pattern; longer values are decoded as a double ([`Field::as_f64`]),
    /// so a sender may use either encoding.
    pub fn as_f32_or_f64(&self) -> Result<f64, TlvError> {
        if self.value.len() > 4 {
            return self.as_f64();
        }
        Ok(f64::from(f32::from_bits(self.as_u32()?)))
    }

    /// C `decode_double`: the (zero-suppressed) `f64` bit pattern. Note that
    /// a 4-byte `float` read this way becomes a tiny denormal, as in C, so
    /// use [`Field::as_f32_or_f64`] for fields radiod encodes as `float`.
    pub fn as_f64(&self) -> Result<f64, TlvError> {
        Ok(f64::from_bits(self.as_u64()?))
    }

    /// The raw value bytes (e.g. `BIN_BYTE_DATA`).
    pub fn as_bytes(&self) -> &'a [u8] {
        self.value
    }

    /// The value as strict UTF-8, zero-copy. No NUL trimming is done;
    /// radiod sends strings without a terminator.
    pub fn as_str(&self) -> Result<&'a str, TlvError> {
        std::str::from_utf8(self.value).map_err(|_| TlvError::InvalidUtf8)
    }

    /// The value as text, replacing invalid UTF-8 with U+FFFD. Use this for
    /// display-only strings such as `DESCRIPTION` and `PRESET`.
    pub fn as_string_lossy(&self) -> Cow<'a, str> {
        String::from_utf8_lossy(self.value)
    }

    /// Socket address (`decode_socket`): 6 bytes are IPv4, 18 bytes IPv6;
    /// any other length (including the "null socket" of length 0 that the
    /// upstream docs allow) is an error.
    pub fn as_socket(&self) -> Result<SocketAddr, TlvError> {
        let v = self.value;
        match (v.len(), v.split_last_chunk::<2>()) {
            (6, Some((ip, port))) => {
                let ip: [u8; 4] = ip.try_into().map_err(|_| TlvError::BadSocketLength(6))?;
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::from(ip),
                    u16::from_be_bytes(*port),
                )))
            }
            (18, Some((ip, port))) => {
                let ip: [u8; 16] = ip.try_into().map_err(|_| TlvError::BadSocketLength(18))?;
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(ip),
                    u16::from_be_bytes(*port),
                    0,
                    0,
                )))
            }
            (n, _) => Err(TlvError::BadSocketLength(n)),
        }
    }

    /// Big-endian `f32` array (`BIN_DATA`); the length must be a multiple
    /// of 4.
    pub fn as_f32_array(&self) -> Result<Vec<f32>, TlvError> {
        if self.value.len() % 4 != 0 {
            return Err(TlvError::BadFloatArrayLength(self.value.len()));
        }
        Ok(self
            .value
            .chunks_exact(4)
            .map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }
}

/// Reads the TLV starting at `pos` of `body` (the packet without its type
/// byte). Returns `None` at `EOL` or at the end of the buffer.
fn read_tlv(body: &[u8], pos: usize) -> Result<Option<(Field<'_>, usize)>, TlvError> {
    let Some(&tag) = body.get(pos) else {
        return Ok(None);
    };
    if tag == StatusType::Eol.value() {
        return Ok(None);
    }
    let offset = pos + 1; // offset in the packet, counting the type byte
    let truncated = TlvError::Truncated { tag, offset };
    let Some(&first) = body.get(pos + 1) else {
        return Err(truncated);
    };
    let mut p = pos + 2;
    let len = if first & 0x80 == 0 {
        usize::from(first)
    } else {
        let n = first & 0x7f;
        if n > MAX_LENGTH_OF_LENGTH {
            return Err(TlvError::LengthOfLength { tag, offset, n });
        }
        let end = p + usize::from(n);
        let len_bytes = body.get(p..end).ok_or(truncated)?;
        p = end;
        len_bytes
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b))
    };
    let remaining = body.len().saturating_sub(p);
    if len > remaining {
        return Err(TlvError::LengthExceedsBuffer {
            tag,
            offset,
            len,
            remaining,
        });
    }
    let value = body.get(p..p + len).unwrap_or_default();
    Ok(Some((Field { tag, value }, p + len)))
}

/// A validated command or status packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Packet type from the first byte.
    pub kind: PktType,
    body: &'a [u8],
}

/// Parses and validates a packet (see the module docs for the rules).
///
/// ```
/// use backend_ka9q::tags::{PktType, StatusType};
/// use backend_ka9q::tlv::parse;
///
/// let pkt = parse(&[0, 18, 2, 0x12, 0x34, 0]).unwrap();
/// assert_eq!(pkt.kind, PktType::Status);
/// let f = pkt.fields().next().unwrap();
/// assert_eq!(f.status_type(), Some(StatusType::OutputSsrc));
/// assert_eq!(f.as_u32().unwrap(), 0x1234);
/// ```
pub fn parse(buf: &[u8]) -> Result<Packet<'_>, TlvError> {
    let (&first, body) = buf.split_first().ok_or(TlvError::Empty)?;
    let kind = PktType::from_u8(first).ok_or(TlvError::UnknownPacketType(first))?;
    let mut pos = 0;
    while let Some((_, next)) = read_tlv(body, pos)? {
        pos = next;
    }
    Ok(Packet { kind, body })
}

impl<'a> Packet<'a> {
    /// Iterates over the fields in packet order, stopping at `EOL`.
    pub fn fields(&self) -> Fields<'a> {
        Fields {
            body: self.body,
            pos: 0,
        }
    }

    /// The last field with this tag, if any (later duplicates win).
    pub fn last(&self, tag: StatusType) -> Option<Field<'a>> {
        self.fields().filter(|f| f.tag == tag.value()).last()
    }

    /// The channel SSRC (`OUTPUT_SSRC`, first occurrence, as upstream
    /// `get_ssrc`), if present and well formed.
    pub fn ssrc(&self) -> Option<u32> {
        self.fields()
            .find(|f| f.tag == StatusType::OutputSsrc.value())
            .and_then(|f| f.as_u32().ok())
    }

    /// The echoed `COMMAND_TAG` (first occurrence, as upstream `get_tag`),
    /// if present and well formed. radiod stores tags as `uint64_t`.
    pub fn command_tag(&self) -> Option<u64> {
        self.fields()
            .find(|f| f.tag == StatusType::CommandTag.value())
            .and_then(|f| f.as_u64().ok())
    }
}

/// Iterator over the fields of a [`Packet`].
#[derive(Debug, Clone)]
pub struct Fields<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Fields<'a> {
    type Item = Field<'a>;

    fn next(&mut self) -> Option<Field<'a>> {
        // The packet was validated by `parse`, so errors cannot happen here;
        // stop cleanly if one does anyway.
        if let Ok(Some((field, next))) = read_tlv(self.body, self.pos) {
            self.pos = next;
            Some(field)
        } else {
            self.pos = self.body.len();
            None
        }
    }
}

impl FusedIterator for Fields<'_> {}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "the codec must be bit-exact")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const T: StatusType = StatusType::OutputSsrc; // tag 18
    const TB: u8 = 18;

    fn status() -> Encoder {
        Encoder::new(PktType::Status)
    }

    fn one(pkt: &[u8]) -> Field<'_> {
        let p = parse(pkt).unwrap();
        let mut it = p.fields();
        let f = it.next().unwrap();
        assert_eq!(it.next(), None);
        f
    }

    // ---- Known answers, hand-derived from status.c ----

    #[test]
    fn integer_known_answers() {
        assert_eq!(status().u64(T, 0).finish(), [0, TB, 0, 0]);
        assert_eq!(status().u64(T, 1).finish(), [0, TB, 1, 1, 0]);
        assert_eq!(status().u64(T, 0x100).finish(), [0, TB, 2, 1, 0, 0]);
        assert_eq!(status().u64(T, 0x1234).finish(), [0, TB, 2, 0x12, 0x34, 0]);
        assert_eq!(
            status().u64(T, u64::MAX).finish(),
            [0, TB, 8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]
        );
        assert_eq!(
            status().u32(T, 0xdead_beef).finish(),
            [0, TB, 4, 0xde, 0xad, 0xbe, 0xef, 0]
        );
        assert_eq!(status().u16(T, 0x00ff).finish(), [0, TB, 1, 0xff, 0]);
        assert_eq!(status().byte(T, 0).finish(), [0, TB, 0, 0]);
        assert_eq!(status().byte(T, 7).finish(), [0, TB, 1, 7, 0]);
        assert_eq!(status().bool(T, false).finish(), [0, TB, 0, 0]);
        assert_eq!(status().bool(T, true).finish(), [0, TB, 1, 1, 0]);
    }

    #[test]
    fn negative_int_is_sign_extended_to_eight_bytes() {
        // encode_int(-1) == encode_int64((uint64_t)-1)
        let pkt = status().i32(T, -1).finish();
        assert_eq!(
            pkt,
            [0, TB, 8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]
        );
        let f = one(&pkt);
        assert_eq!(f.as_i32().unwrap(), -1);
        assert_eq!(f.as_u32().unwrap(), u32::MAX);
        assert_eq!(f.as_i64().unwrap(), -1);

        let pkt = status().i32(T, i32::MIN).finish();
        assert_eq!(pkt, [0, TB, 8, 0xff, 0xff, 0xff, 0xff, 0x80, 0, 0, 0, 0]);
        assert_eq!(one(&pkt).as_i32().unwrap(), i32::MIN);
        assert_eq!(status().i32(T, 5).finish(), [0, TB, 1, 5, 0]);
        assert_eq!(
            status().i64(T, -2).finish(),
            [0, TB, 8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0]
        );
    }

    #[test]
    fn float_known_answers() {
        assert_eq!(
            status().f32(T, 1.0).finish(),
            [0, TB, 4, 0x3f, 0x80, 0, 0, 0]
        );
        assert_eq!(status().f32(T, 0.0).finish(), [0, TB, 0, 0]);
        assert_eq!(status().f32(T, -0.0).finish(), [0, TB, 4, 0x80, 0, 0, 0, 0]);
        assert_eq!(
            status().f32(T, -2.5).finish(),
            [0, TB, 4, 0xc0, 0x20, 0, 0, 0]
        );
        // Smallest positive denormal: bit pattern 1, zero-suppressed to 1 byte.
        let tiny = f64::from(f32::from_bits(1));
        let pkt = status().f32(T, tiny).finish();
        assert_eq!(pkt, [0, TB, 1, 1, 0]);
        assert_eq!(one(&pkt).as_f32_or_f64().unwrap(), tiny);
        // NaN is never encoded.
        assert_eq!(status().f32(T, f64::NAN).finish(), [0, 0]);
        // Infinity is.
        assert_eq!(
            status().f32(T, f64::INFINITY).finish(),
            [0, TB, 4, 0x7f, 0x80, 0, 0, 0]
        );
    }

    #[test]
    fn double_known_answers() {
        assert_eq!(
            status().f64(T, 1.0).finish(),
            [0, TB, 8, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0, 0]
        );
        // 10 MHz = 0x416312D000000000
        assert_eq!(
            status().f64(T, 10e6).finish(),
            [0, TB, 8, 0x41, 0x63, 0x12, 0xd0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            status().f64(T, -2.0).finish(),
            [0, TB, 8, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(status().f64(T, 0.0).finish(), [0, TB, 0, 0]);
        assert_eq!(status().f64(T, f64::NAN).finish(), [0, 0]);
        let tiny = f64::from_bits(0x10);
        assert_eq!(status().f64(T, tiny).finish(), [0, TB, 1, 0x10, 0]);
    }

    #[test]
    fn decode_float_semantics() {
        // len 0 -> 0.0
        assert_eq!(one(&[0, TB, 0]).as_f32_or_f64().unwrap(), 0.0);
        // 4 bytes -> float
        assert_eq!(
            one(&[0, TB, 4, 0x3f, 0x80, 0, 0]).as_f32_or_f64().unwrap(),
            1.0
        );
        // 3 bytes -> zero-suppressed float bit pattern
        assert_eq!(
            one(&[0, TB, 3, 1, 0, 0]).as_f32_or_f64().unwrap(),
            f64::from(f32::from_bits(0x1_0000))
        );
        // > 4 bytes -> decoded as a double
        let d = [0, TB, 8, 0x41, 0x63, 0x12, 0xd0, 0, 0, 0, 0];
        assert_eq!(one(&d).as_f32_or_f64().unwrap(), 10e6);
        assert_eq!(one(&d).as_f64().unwrap(), 10e6);
        // A float read as a double is a tiny denormal, as in C.
        let f = [0, TB, 4, 0x3f, 0x80, 0, 0];
        assert_eq!(one(&f).as_f64().unwrap(), f64::from_bits(0x3f80_0000));
        assert_eq!(one(&[0, TB, 0]).as_f64().unwrap(), 0.0);
        // Longer than 8 bytes is rejected.
        let long = [0, TB, 9, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(one(&long).as_f32_or_f64(), Err(TlvError::IntegerTooLong(9)));
        assert_eq!(one(&long).as_f64(), Err(TlvError::IntegerTooLong(9)));
    }

    #[test]
    fn decode_integer_masking() {
        let f = one(&[0, TB, 5, 0x01, 0x00, 0x01, 0x23, 0x45]);
        assert_eq!(f.as_u64().unwrap(), 0x01_0001_2345);
        assert_eq!(f.as_u32().unwrap(), 0x0001_2345);
        assert_eq!(f.as_u16().unwrap(), 0x2345);
        assert_eq!(f.as_u8().unwrap(), 0x45);
        assert_eq!(f.as_i32().unwrap(), 0x0001_2345);
        assert!(f.as_bool().unwrap());
        let zero = one(&[0, TB, 0]);
        assert_eq!(zero.as_u64().unwrap(), 0);
        assert!(!zero.as_bool().unwrap());
        // Non-minimal encodings decode fine.
        assert!(!one(&[0, TB, 2, 0, 0]).as_bool().unwrap());
        assert!(one(&[0, TB, 2, 0, 1]).as_bool().unwrap());
        let long = one(&[0, TB, 9, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        for r in [
            long.as_u64().map(|_| ()),
            long.as_u32().map(|_| ()),
            long.as_u16().map(|_| ()),
            long.as_u8().map(|_| ()),
            long.as_bool().map(|_| ()),
            long.as_i32().map(|_| ()),
            long.as_i64().map(|_| ()),
        ] {
            assert_eq!(r, Err(TlvError::IntegerTooLong(9)));
        }
    }

    #[test]
    fn string_length_forms() {
        let t = StatusType::Description; // 4
        assert_eq!(status().str(t, "").finish(), [0, 4, 0, 0]);
        assert_eq!(status().str(t, "hi").finish(), [0, 4, 2, b'h', b'i', 0]);

        let s127 = vec![b'a'; 127];
        let pkt = status().bytes(t, &s127).finish();
        assert_eq!(pkt[..3], [0, 4, 127]);
        assert_eq!(pkt.len(), 3 + 127 + 1);

        let s128 = vec![b'b'; 128];
        let pkt = status().bytes(t, &s128).finish();
        assert_eq!(pkt[..5], [0, 4, 0x82, 0x00, 0x80]);
        assert_eq!(one(&pkt).as_bytes(), &s128[..]);

        let s65535 = vec![7u8; 65535];
        let pkt = status().bytes(t, &s65535).finish();
        assert_eq!(pkt[..5], [0, 4, 0x82, 0xff, 0xff]);
        assert_eq!(one(&pkt).as_bytes().len(), 65535);

        let s65536 = vec![9u8; 65536];
        let pkt = status().bytes(t, &s65536).finish();
        assert_eq!(pkt[..6], [0, 4, 0x83, 0x01, 0x00, 0x00]);
        assert_eq!(one(&pkt).as_bytes(), &s65536[..]);

        let big = vec![1u8; 0x100_0000];
        let pkt = status().bytes(t, &big).finish();
        assert_eq!(pkt[..7], [0, 4, 0x84, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(one(&pkt).as_bytes().len(), 0x100_0000);
    }

    #[test]
    fn string_decoding() {
        let f = one(&[0, 4, 3, b'u', b's', b'b']);
        assert_eq!(f.as_str().unwrap(), "usb");
        assert_eq!(f.as_string_lossy(), "usb");
        let bad = one(&[0, 4, 2, 0xff, b'a']);
        assert_eq!(bad.as_str(), Err(TlvError::InvalidUtf8));
        assert_eq!(bad.as_string_lossy(), "\u{fffd}a");
    }

    #[test]
    fn socket_known_answers() {
        let v4: SocketAddr = "239.1.2.3:5004".parse().unwrap();
        let pkt = status().socket(T, v4).finish();
        assert_eq!(pkt, [0, TB, 6, 239, 1, 2, 3, 0x13, 0x8c, 0]);
        assert_eq!(one(&pkt).as_socket().unwrap(), v4);

        let v6: SocketAddr = "[ff02::1]:5006".parse().unwrap();
        let pkt = status().socket(T, v6).finish();
        let mut want = vec![0, TB, 18, 0xff, 0x02];
        want.extend_from_slice(&[0; 13]);
        want.extend_from_slice(&[1, 0x13, 0x8e, 0]);
        assert_eq!(pkt, want);
        assert_eq!(one(&pkt).as_socket().unwrap(), v6);

        for n in [0usize, 5, 7, 17, 19] {
            let mut pkt = vec![0, TB, u8::try_from(n).unwrap()];
            pkt.extend(std::iter::repeat_n(1, n));
            assert_eq!(one(&pkt).as_socket(), Err(TlvError::BadSocketLength(n)));
        }
    }

    #[test]
    fn float_array() {
        let pkt = status()
            .f32_array(StatusType::BinData, &[1.0, -2.5])
            .finish();
        assert_eq!(pkt, [0, 96, 8, 0x3f, 0x80, 0, 0, 0xc0, 0x20, 0, 0, 0]);
        assert_eq!(one(&pkt).as_f32_array().unwrap(), [1.0, -2.5]);
        assert_eq!(
            status().f32_array(StatusType::BinData, &[]).finish(),
            [0, 96, 0, 0]
        );
        let pkt = status().f32_array(StatusType::BinData, &[0.0; 32]).finish();
        assert_eq!(pkt[..5], [0, 96, 0x82, 0, 128]);
        assert_eq!(
            one(&[0, 96, 6, 0, 0, 0, 0, 0, 0]).as_f32_array(),
            Err(TlvError::BadFloatArrayLength(6))
        );
    }

    // ---- Packet structure ----

    #[test]
    fn packet_types() {
        assert_eq!(parse(&[]), Err(TlvError::Empty));
        assert_eq!(parse(&[2]), Err(TlvError::UnknownPacketType(2)));
        let p = parse(&[1]).unwrap();
        assert_eq!(p.kind, PktType::Cmd);
        assert_eq!(p.fields().count(), 0);
        assert_eq!(parse(&[0, 0]).unwrap().kind, PktType::Status);
    }

    #[test]
    fn missing_eol_is_tolerated_and_trailing_bytes_ignored() {
        let p = parse(&[0, TB, 1, 5]).unwrap();
        assert_eq!(p.fields().count(), 1);
        // Garbage after EOL is ignored, even if it is not valid TLV.
        let p = parse(&[0, TB, 1, 5, 0, 0xff, 0x85]).unwrap();
        assert_eq!(p.fields().count(), 1);
    }

    #[test]
    fn truncated_and_oversize_lengths() {
        // Tag without a length byte.
        assert_eq!(
            parse(&[0, TB, 1, 5, 33]),
            Err(TlvError::Truncated { tag: 33, offset: 4 })
        );
        // Value runs past the end.
        assert_eq!(
            parse(&[0, TB, 5, 1, 2]),
            Err(TlvError::LengthExceedsBuffer {
                tag: TB,
                offset: 1,
                len: 5,
                remaining: 2
            })
        );
        // Long form with n > 4.
        assert_eq!(
            parse(&[0, TB, 0x85, 0, 0, 0, 0, 1, 0]),
            Err(TlvError::LengthOfLength {
                tag: TB,
                offset: 1,
                n: 5
            })
        );
        // Long form with missing length bytes.
        assert_eq!(
            parse(&[0, TB, 0x82, 0x01]),
            Err(TlvError::Truncated { tag: TB, offset: 1 })
        );
        // Long form declaring 4 GiB.
        assert!(matches!(
            parse(&[0, TB, 0x84, 0xff, 0xff, 0xff, 0xff, 0]),
            Err(TlvError::LengthExceedsBuffer {
                len: 0xffff_ffff,
                ..
            })
        ));
        // 0x80 (zero length bytes) means length 0, as in C.
        let p = parse(&[0, TB, 0x80, 0]).unwrap();
        assert_eq!(p.fields().next().unwrap().value, &[] as &[u8]);
        // Long form may encode short lengths.
        let p = parse(&[0, TB, 0x81, 2, 0xab, 0xcd, 0]).unwrap();
        assert_eq!(p.fields().next().unwrap().as_u16().unwrap(), 0xabcd);
    }

    #[test]
    fn unknown_tags_are_skipped() {
        let pkt = [0, 200, 2, 9, 9, TB, 1, 42, 0];
        let p = parse(&pkt).unwrap();
        let fields: Vec<_> = p.fields().collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].tag, 200);
        assert_eq!(fields[0].status_type(), None);
        assert_eq!(fields[1].status_type(), Some(T));
        assert_eq!(p.ssrc(), Some(42));
    }

    #[test]
    fn last_ssrc_and_command_tag() {
        let pkt = Encoder::new(PktType::Status)
            .u32(StatusType::OutputSsrc, 1)
            .u64(StatusType::CommandTag, 0x1_0000_0000)
            .u32(StatusType::OutputSsrc, 2)
            .finish();
        let p = parse(&pkt).unwrap();
        assert_eq!(p.ssrc(), Some(1), "first occurrence, like get_ssrc");
        assert_eq!(p.last(StatusType::OutputSsrc).unwrap().as_u32().unwrap(), 2);
        assert_eq!(p.command_tag(), Some(0x1_0000_0000));
        assert_eq!(p.last(StatusType::Preset), None);
        let empty = parse(&[1, 0]).unwrap();
        assert_eq!(empty.ssrc(), None);
        assert_eq!(empty.command_tag(), None);
    }

    #[test]
    fn fields_iterator_is_fused() {
        let pkt = [0, TB, 0, 0];
        let p = parse(&pkt).unwrap();
        let mut it = p.fields();
        assert!(it.next().is_some());
        assert!(it.next().is_none());
        assert!(it.next().is_none());
    }

    #[test]
    fn error_display() {
        let e = TlvError::Field {
            tag: StatusType::LowEdge,
            source: Box::new(TlvError::IntegerTooLong(9)),
        };
        assert_eq!(
            e.to_string(),
            "invalid LOW_EDGE field: integer value of 9 bytes is longer than 8"
        );
        assert_eq!(
            TlvError::WrongPacketType {
                expected: PktType::Status,
                got: PktType::Cmd
            }
            .to_string(),
            "expected a STATUS packet, got CMD"
        );
    }

    // ---- Property tests ----

    fn any_tag() -> impl Strategy<Value = StatusType> {
        (1..StatusType::ALL.len()).prop_map(|i| StatusType::ALL[i])
    }

    proptest! {
        #[test]
        fn roundtrip_u64(v: u64) {
            let pkt = status().u64(T, v).finish();
            prop_assert_eq!(one(&pkt).as_u64().unwrap(), v);
            // Minimal length: zero is empty, otherwise no leading zero byte.
            let f = one(&pkt);
            prop_assert!(f.value.first() != Some(&0));
        }

        #[test]
        fn roundtrip_small_ints(a: u32, b: u16, c: u8, d: bool) {
            let pkt = status()
                .u32(T, a)
                .u16(T, b)
                .byte(T, c)
                .bool(T, d)
                .finish();
            let p = parse(&pkt).unwrap();
            let f: Vec<_> = p.fields().collect();
            prop_assert_eq!(f[0].as_u32().unwrap(), a);
            prop_assert_eq!(f[1].as_u16().unwrap(), b);
            prop_assert_eq!(f[2].as_u8().unwrap(), c);
            prop_assert_eq!(f[3].as_bool().unwrap(), d);
        }

        #[test]
        fn roundtrip_signed(a: i32, b: i64) {
            let pkt = status().i32(T, a).i64(T, b).finish();
            let p = parse(&pkt).unwrap();
            let f: Vec<_> = p.fields().collect();
            prop_assert_eq!(f[0].as_i32().unwrap(), a);
            prop_assert_eq!(f[1].as_i64().unwrap(), b);
        }

        #[test]
        fn roundtrip_f32(bits: u32) {
            let v = f32::from_bits(bits);
            let pkt = status().f32(T, f64::from(v)).finish();
            if v.is_nan() {
                prop_assert_eq!(pkt, vec![0, 0]);
            } else {
                let got = one(&pkt).as_f32_or_f64().unwrap();
                prop_assert_eq!(got.to_bits(), f64::from(v).to_bits());
            }
        }

        #[test]
        fn roundtrip_f64(bits: u64) {
            let v = f64::from_bits(bits);
            let pkt = status().f64(T, v).finish();
            if v.is_nan() {
                prop_assert_eq!(pkt, vec![0, 0]);
            } else {
                prop_assert_eq!(one(&pkt).as_f64().unwrap().to_bits(), bits);
                // decode_float falls back to double for > 4 bytes, so it
                // agrees whenever the double needed more than 4 bytes.
                if one(&pkt).value.len() > 4 {
                    prop_assert_eq!(one(&pkt).as_f32_or_f64().unwrap().to_bits(), bits);
                }
            }
        }

        #[test]
        fn roundtrip_bytes(v in proptest::collection::vec(any::<u8>(), 0..70_000)) {
            let pkt = status().bytes(StatusType::BinByteData, &v).finish();
            prop_assert_eq!(one(&pkt).as_bytes(), &v[..]);
        }

        #[test]
        fn roundtrip_str(s in ".{0,300}") {
            let pkt = status().str(StatusType::Preset, &s).finish();
            prop_assert_eq!(one(&pkt).as_str().unwrap(), s.as_str());
        }

        #[test]
        fn roundtrip_socket_v4(ip: [u8; 4], port: u16) {
            let a = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port));
            let pkt = status().socket(T, a).finish();
            prop_assert_eq!(one(&pkt).as_socket().unwrap(), a);
        }

        #[test]
        fn roundtrip_socket_v6(ip: [u8; 16], port: u16) {
            let a = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0));
            let pkt = status().socket(T, a).finish();
            prop_assert_eq!(one(&pkt).as_socket().unwrap(), a);
        }

        #[test]
        fn roundtrip_f32_array(bits in proptest::collection::vec(any::<u32>(), 0..600)) {
            let v: Vec<f32> = bits.iter().map(|b| f32::from_bits(*b)).collect();
            let pkt = status().f32_array(StatusType::BinData, &v).finish();
            let got: Vec<u32> = one(&pkt).as_f32_array().unwrap().iter().map(|f| f.to_bits()).collect();
            prop_assert_eq!(got, bits);
        }

        #[test]
        fn roundtrip_many_fields(
            items in proptest::collection::vec(
                (any_tag(), proptest::collection::vec(any::<u8>(), 0..200)), 0..20)
        ) {
            let mut e = Encoder::new(PktType::Cmd);
            for (tag, v) in &items {
                e = e.bytes(*tag, v);
            }
            let pkt = e.finish();
            let p = parse(&pkt).unwrap();
            prop_assert_eq!(p.kind, PktType::Cmd);
            let got: Vec<(u8, Vec<u8>)> = p.fields().map(|f| (f.tag, f.value.to_vec())).collect();
            let want: Vec<(u8, Vec<u8>)> = items.iter().map(|(t, v)| (t.value(), v.clone())).collect();
            prop_assert_eq!(got, want);
        }

        #[test]
        fn never_panics_on_arbitrary_input(buf in proptest::collection::vec(any::<u8>(), 0..512)) {
            if let Ok(p) = parse(&buf) {
                let _ = p.ssrc();
                let _ = p.command_tag();
                let _ = p.last(StatusType::Preset);
                for f in p.fields() {
                    let _ = f.status_type();
                    let _ = f.as_u64();
                    let _ = f.as_u32();
                    let _ = f.as_u16();
                    let _ = f.as_u8();
                    let _ = f.as_bool();
                    let _ = f.as_i32();
                    let _ = f.as_i64();
                    let _ = f.as_f32_or_f64();
                    let _ = f.as_f64();
                    let _ = f.as_bytes();
                    let _ = f.as_str();
                    let _ = f.as_string_lossy();
                    let _ = f.as_socket();
                    let _ = f.as_f32_array();
                }
            }
        }

        #[test]
        fn truncating_a_valid_packet_never_panics(
            items in proptest::collection::vec(
                (any_tag(), proptest::collection::vec(any::<u8>(), 0..300)), 1..6),
            cut in any::<prop::sample::Index>()
        ) {
            let mut e = Encoder::new(PktType::Status);
            for (tag, v) in &items {
                e = e.bytes(*tag, v);
            }
            let pkt = e.finish();
            let cut = cut.index(pkt.len());
            // Either a clean prefix (ends at a TLV boundary) or an error.
            if let Ok(p) = parse(&pkt[..cut]) {
                prop_assert!(p.fields().count() <= items.len());
            }
        }
    }
}
