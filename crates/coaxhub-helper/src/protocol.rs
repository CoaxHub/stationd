// SPDX-License-Identifier: GPL-3.0-only
//! The helper's protocol: one request line, one reply line.
//!
//! Request: ASCII, at most [`MAX_REQUEST_BYTES`] bytes with its newline,
//! `<verb> <id>\n` for `start`, `restart` and `stop`, or `ping\n`. The id is
//! the receiver's number in canonical decimal (`[1-9][0-9]*`, at most
//! `u32::MAX`): `011` would name a different unit instance from `11`.
//! Exactly one space, nothing after the id, no carriage return.
//!
//! Reply: `ok\n`, or `error <detail>\n` (the detail is one line, at most
//! [`MAX_DETAIL_BYTES`] bytes).

use std::fmt;
use std::time::Duration;

/// Where `coaxhub-helper.socket` listens.
pub const SOCKET_PATH: &str = "/run/coaxhub-helper.sock";

/// A request's longest line, newline included.
pub const MAX_REQUEST_BYTES: usize = 64;

/// How long the helper waits for the request line.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest detail of an `error` reply, in bytes.
pub const MAX_DETAIL_BYTES: usize = 256;

/// The longest an instance of the helper lives: `RuntimeMaxSec=15` in
/// `coaxhub-helper@.service` ends it. Its own work is bounded below that
/// (the request within [`REQUEST_TIMEOUT`], then at most two `systemctl`
/// calls of [`crate::server::SYSTEMCTL_TIMEOUT`] each), and a client waits
/// longer than this, so it never gives up on a request still being carried
/// out.
pub const RUNTIME_MAX: Duration = Duration::from_secs(15);

/// The most receivers a station has (the station protocol's
/// `MAX_RECEIVERS`; stationd's tests check that they agree). The helper
/// starts or restarts a receiver's unit only while fewer than this many
/// *other* `coaxhub-radiod@*` units are running or starting, so that a
/// hijacked stationd cannot make systemd run units for ids without limit.
pub const MAX_RECEIVER_UNITS: usize = 16;

/// The reply's detail when the peer is not allowed to ask.
pub const NOT_PERMITTED: &str = "not permitted";

/// What the helper does to a receiver's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `systemctl start` (nothing if it runs).
    Start,
    /// `systemctl restart`.
    Restart,
    /// `systemctl stop`.
    Stop,
}

impl Verb {
    /// The verb as written in a request and given to `systemctl`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Restart => "restart",
            Self::Stop => "stop",
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A well-formed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Does nothing; answers `ok` to a permitted peer.
    Ping,
    /// `verb` on `coaxhub-radiod@<id>.service`.
    Unit {
        /// What to do.
        verb: Verb,
        /// The receiver's number (1 to `u32::MAX`).
        id: u32,
    },
}

/// Why a request line is refused (for the journal; the reply only says
/// `bad request`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Longer than [`MAX_REQUEST_BYTES`], or no newline within them.
    TooLong,
    /// Not ending in a newline.
    NoNewline,
    /// A byte that is not printable ASCII or a space (NUL, CR, UTF-8, ...).
    BadByte,
    /// Not `start`, `restart`, `stop` or `ping`.
    UnknownVerb,
    /// A unit verb without an id, or `ping` with one.
    Shape,
    /// More than one space, or a field after the id.
    ExtraField,
    /// Not canonical decimal (empty, a sign, a leading zero, a non-digit).
    BadId,
    /// Zero, or above `u32::MAX`.
    IdRange,
}

impl Refusal {
    /// A fixed description for the journal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLong => "line too long",
            Self::NoNewline => "no newline",
            Self::BadByte => "byte outside printable ASCII",
            Self::UnknownVerb => "unknown verb",
            Self::Shape => "wrong fields for the verb",
            Self::ExtraField => "extra field",
            Self::BadId => "id is not canonical decimal",
            Self::IdRange => "id out of range",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Request {
    /// Parses one request line, newline included.
    ///
    /// # Errors
    /// The [`Refusal`] when the line is not exactly a request.
    pub fn parse(line: &[u8]) -> Result<Self, Refusal> {
        if line.len() > MAX_REQUEST_BYTES {
            return Err(Refusal::TooLong);
        }
        let body = line.strip_suffix(b"\n").ok_or(Refusal::NoNewline)?;
        if !body.iter().all(|b| matches!(b, 0x20..=0x7e)) {
            return Err(Refusal::BadByte);
        }
        // Printable ASCII only, so this is valid UTF-8.
        let body = std::str::from_utf8(body).map_err(|_| Refusal::BadByte)?;
        let mut fields = body.split(' ');
        let verb = match fields.next().unwrap_or("") {
            "ping" => {
                return if fields.next().is_some() {
                    Err(Refusal::Shape)
                } else {
                    Ok(Self::Ping)
                };
            }
            "start" => Verb::Start,
            "restart" => Verb::Restart,
            "stop" => Verb::Stop,
            _ => return Err(Refusal::UnknownVerb),
        };
        let id = fields.next().ok_or(Refusal::Shape)?;
        if fields.next().is_some() {
            return Err(Refusal::ExtraField);
        }
        Ok(Self::Unit {
            verb,
            id: parse_id(id)?,
        })
    }

    /// The request as sent: the line, newline included.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Ping => "ping\n".to_owned(),
            Self::Unit { verb, id } => format!("{verb} {id}\n"),
        }
    }
}

/// A receiver id in canonical decimal, 1 to `u32::MAX`.
fn parse_id(text: &str) -> Result<u32, Refusal> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) || text.starts_with('0') {
        // "0" itself starts with '0': it is out of range, not malformed.
        return if text == "0" {
            Err(Refusal::IdRange)
        } else {
            Err(Refusal::BadId)
        };
    }
    // Ten digits fit u32::MAX; more is out of range without parsing.
    if text.len() > 10 {
        return Err(Refusal::IdRange);
    }
    text.parse::<u32>().map_err(|_| Refusal::IdRange)
}

/// The helper's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Done (or, for `--no-block`, queued).
    Ok,
    /// Refused or failed: why, one line.
    Error(String),
}

impl Reply {
    /// The reply as sent: the line, newline included.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Ok => "ok\n".to_owned(),
            Self::Error(detail) => format!("error {}\n", sanitize_detail(detail)),
        }
    }

    /// Parses the helper's answer; `None` when it is neither `ok` nor
    /// `error <detail>`.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\n', '\r']);
        if line == "ok" {
            Some(Self::Ok)
        } else {
            line.strip_prefix("error ")
                .map(|d| Self::Error(d.to_owned()))
        }
    }
}

/// One line of at most [`MAX_DETAIL_BYTES`] bytes: control characters
/// become spaces, runs of whitespace one space, cut on a character
/// boundary.
#[must_use]
pub fn sanitize_detail(text: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in text.chars() {
        if c.is_control() || c.is_whitespace() {
            gap = !out.is_empty();
        } else {
            if gap {
                out.push(' ');
                gap = false;
            }
            out.push(c);
        }
    }
    if out.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(verb: Verb, id: u32) -> Request {
        Request::Unit { verb, id }
    }

    #[test]
    fn accepts_the_four_verbs() {
        assert_eq!(Request::parse(b"ping\n"), Ok(Request::Ping));
        assert_eq!(Request::parse(b"start 11\n"), Ok(unit(Verb::Start, 11)));
        assert_eq!(Request::parse(b"restart 1\n"), Ok(unit(Verb::Restart, 1)));
        assert_eq!(Request::parse(b"stop 7\n"), Ok(unit(Verb::Stop, 7)));
        assert_eq!(
            Request::parse(b"start 4294967295\n"),
            Ok(unit(Verb::Start, u32::MAX))
        );
    }

    #[test]
    fn refuses_hostile_input() {
        let long = format!("start {}\n", "1".repeat(70));
        let table: &[(&[u8], Refusal)] = &[
            (b"", Refusal::NoNewline),
            (b"\n", Refusal::UnknownVerb),
            (b"start 11", Refusal::NoNewline),
            (b"start 11\r\n", Refusal::BadByte),
            (b"enable 11\n", Refusal::UnknownVerb),
            (b"daemon-reload\n", Refusal::UnknownVerb),
            (b"START 11\n", Refusal::UnknownVerb),
            (b"Start 11\n", Refusal::UnknownVerb),
            (b" start 11\n", Refusal::UnknownVerb),
            (b"start\n", Refusal::Shape),
            (b"ping 1\n", Refusal::Shape),
            (b"ping \n", Refusal::Shape),
            (b"start 0\n", Refusal::IdRange),
            (b"start 00\n", Refusal::BadId),
            (b"start 011\n", Refusal::BadId),
            (b"start 11 12\n", Refusal::ExtraField),
            (b"start 11 \n", Refusal::ExtraField),
            (b"start  11\n", Refusal::ExtraField),
            (b"start 11\n\n", Refusal::BadByte),
            (b"start coaxhub-radiod@11\n", Refusal::BadId),
            (b"start coaxhub-radiod@11.service\n", Refusal::BadId),
            (b"start 4294967296\n", Refusal::IdRange),
            (b"start 99999999999999999999\n", Refusal::IdRange),
            (b"start -1\n", Refusal::BadId),
            (b"start +1\n", Refusal::BadId),
            (b"start 1.0\n", Refusal::BadId),
            (b"start 0x10\n", Refusal::BadId),
            (b"start 1e3\n", Refusal::BadId),
            (b"start \xef\xbc\x91\n", Refusal::BadByte),
            (b"start 1\x00\n", Refusal::BadByte),
            (b"start\x001\n", Refusal::BadByte),
            (b"start\t1\n", Refusal::BadByte),
            (b"start 1\xff\n", Refusal::BadByte),
            (b"start ../x\n", Refusal::BadId),
            (b"start 1;reboot\n", Refusal::BadId),
            (b"start $(id)\n", Refusal::BadId),
            (long.as_bytes(), Refusal::TooLong),
        ];
        for (line, want) in table {
            assert_eq!(
                Request::parse(line),
                Err(*want),
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
        let huge = vec![b'a'; 1 << 20];
        assert_eq!(Request::parse(&huge), Err(Refusal::TooLong));
    }

    #[test]
    fn the_length_limit_is_64_bytes_with_the_newline() {
        // "start " is 6 bytes, the newline 1: a 10-digit id is 17 in all.
        let ok = format!("start {}\n", u32::MAX);
        assert!(ok.len() <= MAX_REQUEST_BYTES);
        assert!(Request::parse(ok.as_bytes()).is_ok());
        let edge = vec![b'x'; MAX_REQUEST_BYTES];
        assert_eq!(Request::parse(&edge), Err(Refusal::NoNewline));
        let over = vec![b'x'; MAX_REQUEST_BYTES + 1];
        assert_eq!(Request::parse(&over), Err(Refusal::TooLong));
    }

    #[test]
    fn requests_round_trip() {
        for r in [
            Request::Ping,
            unit(Verb::Start, 1),
            unit(Verb::Restart, 12345),
            unit(Verb::Stop, u32::MAX),
        ] {
            assert_eq!(Request::parse(r.encode().as_bytes()), Ok(r));
        }
    }

    #[test]
    fn replies() {
        assert_eq!(Reply::Ok.encode(), "ok\n");
        assert_eq!(Reply::parse("ok\n"), Some(Reply::Ok));
        assert_eq!(
            Reply::parse("error bad request\n"),
            Some(Reply::Error("bad request".into()))
        );
        assert_eq!(Reply::parse("okay\n"), None);
        assert_eq!(Reply::parse(""), None);
        // A detail is one line, whatever systemctl printed.
        assert_eq!(
            Reply::Error("a\nb\u{0}\tc  d\r\n".into()).encode(),
            "error a b c d\n"
        );
        let long = Reply::Error("é".repeat(400)).encode();
        assert!(
            long.len() <= "error ".len() + MAX_DETAIL_BYTES + 1,
            "{}",
            long.len()
        );
        assert!(long.ends_with("é\n"));
        assert_eq!(long.matches('\n').count(), 1);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            // Never panics, and accepts only what `encode` would write.
            #[test]
            fn only_canonical_lines_pass(line in proptest::collection::vec(any::<u8>(), 0..100)) {
                if let Ok(r) = Request::parse(&line) {
                    let text = r.encode();
                    prop_assert_eq!(text.as_bytes(), line.as_slice());
                }
            }

            #[test]
            fn every_valid_id_round_trips(id in 1..=u32::MAX, verb in 0..3usize) {
                let verb = [Verb::Start, Verb::Restart, Verb::Stop][verb];
                let line = format!("{verb} {id}\n");
                prop_assert_eq!(Request::parse(line.as_bytes()), Ok(Request::Unit { verb, id }));
            }

            #[test]
            fn detail_is_one_short_line(text in ".*") {
                let d = sanitize_detail(&text);
                prop_assert!(d.len() <= MAX_DETAIL_BYTES);
                prop_assert!(!d.chars().any(char::is_control));
            }
        }
    }
}
