// SPDX-License-Identifier: GPL-3.0-only
//! The helper's side of a connection: who may ask, reading the request
//! within its time, running `systemctl`, answering.
//!
//! Order of events ([`handle`]): the peer's UID is checked before a byte is
//! read; the request is read byte by byte up to
//! [`MAX_REQUEST_BYTES`](crate::protocol::MAX_REQUEST_BYTES) and
//! [`REQUEST_TIMEOUT`](crate::protocol::REQUEST_TIMEOUT); [`Request::parse`] decides whether it is a request at
//! all; only then does anything run. The helper writes no file, and reads
//! nothing `stationd` can write (only `/etc/passwd`, once).

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::protocol::{
    MAX_RECEIVER_UNITS, MAX_REQUEST_BYTES, NOT_PERMITTED, Refusal, Reply, Request, Verb,
};

/// The user whose UID may connect (besides root).
pub const STATIOND_USER: &str = "coaxhub";

/// How long a `systemctl` call may take. A request makes at most two (the
/// count of the other units, then the verb): with the 2 s to read the
/// request, 12 s at the most, under [`crate::protocol::RUNTIME_MAX`].
pub const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a refused peer's data is read and thrown away after the
/// reply, so that closing the socket does not reset the connection before
/// the peer has read the answer.
const DRAIN_BYTES: usize = 64 * 1024;

/// The unit of receiver `id`.
#[must_use]
pub fn unit_name(id: u32) -> String {
    format!("coaxhub-radiod@{id}.service")
}

/// Who may talk to the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The UID of `coaxhub`; `None` when it could not be found (then only
    /// root may talk).
    pub coaxhub_uid: Option<u32>,
}

impl Policy {
    /// Whether a peer with this UID may send requests: `coaxhub` and root
    /// (`sudo stationd doctor` pings).
    #[must_use]
    pub fn permits(&self, uid: u32) -> bool {
        uid == 0 || self.coaxhub_uid == Some(uid)
    }
}

/// The UID of `user` in the text of a `passwd` file.
#[must_use]
pub fn uid_of(passwd: &str, user: &str) -> Option<u32> {
    passwd.lines().find_map(|line| {
        let mut f = line.split(':');
        (f.next()? == user).then_some(())?;
        f.nth(1)?.parse().ok()
    })
}

/// Does the work a request asks for.
pub trait Runner {
    /// Runs `verb` on receiver `id`'s unit; `Err` is the one-line reason.
    ///
    /// # Errors
    /// The detail for the `error` reply.
    fn run(&self, verb: Verb, id: u32) -> Result<(), String>;

    /// How many `coaxhub-radiod@*` units other than receiver `id`'s are
    /// running or on their way (active, activating, which includes waiting
    /// to restart, or deactivating), from systemd itself: the helper reads no
    /// file stationd writes. A failed unit runs nothing and is not counted
    /// (`stop` does not clear it, so counting it could fill the limit with
    /// units nobody can remove).
    ///
    /// # Errors
    /// The detail for the `error` reply.
    fn busy_others(&self, id: u32) -> Result<usize, String>;
}

/// `systemctl --no-block <verb> coaxhub-radiod@<id>.service`, with an empty
/// environment.
#[derive(Debug, Clone)]
pub struct Systemctl {
    /// Fixed paths to try, in order (`/bin` for a system that is not
    /// merged-`/usr`).
    pub candidates: Vec<PathBuf>,
    /// How long it may take before it is killed.
    pub timeout: Duration,
}

impl Default for Systemctl {
    fn default() -> Self {
        Self {
            candidates: vec!["/usr/bin/systemctl".into(), "/bin/systemctl".into()],
            timeout: SYSTEMCTL_TIMEOUT,
        }
    }
}

/// A thread that reads up to 64 KiB of `pipe` as text.
fn read_pipe(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(p) = pipe {
            let _ = p.take(64 * 1024).read_to_string(&mut text);
        }
        text
    })
}

impl Systemctl {
    /// Runs systemctl with `args` (and an empty environment), killed at the
    /// timeout; its standard output on success. `shown` names the call in
    /// errors.
    fn exec(&self, args: &[&str], shown: &str) -> Result<String, String> {
        let path = self
            .candidates
            .iter()
            .find(|p| p.exists())
            .ok_or_else(|| format!("{shown}: no systemctl found"))?;
        let mut child = Command::new(path)
            .arg("--no-ask-password")
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{shown}: cannot run systemctl: {e}"))?;
        let out = read_pipe(child.stdout.take());
        let err = read_pipe(child.stderr.take());
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out.join();
                    let _ = err.join();
                    return Err(format!("{shown}: timed out"));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => break Err(e),
            }
        };
        let stdout = out.join().unwrap_or_default();
        let stderr = err.join().unwrap_or_default();
        match status {
            Ok(s) if s.success() => Ok(stdout),
            Ok(s) => {
                let why = stderr.trim();
                Err(if why.is_empty() {
                    format!("{shown}: {s}")
                } else {
                    format!("{shown}: {why}")
                })
            }
            Err(e) => Err(format!("{shown}: {e}")),
        }
    }
}

impl Runner for Systemctl {
    fn run(&self, verb: Verb, id: u32) -> Result<(), String> {
        let unit = unit_name(id);
        let shown = format!("systemctl --no-block {verb} {unit}");
        self.exec(&["--no-block", verb.as_str(), &unit], &shown)
            .map(drop)
    }

    fn busy_others(&self, id: u32) -> Result<usize, String> {
        let own = unit_name(id);
        let text = self.exec(
            &[
                "list-units",
                "coaxhub-radiod@*.service",
                "--state=active,activating,deactivating",
                "--plain",
                "--no-legend",
                "--no-pager",
                "--full",
            ],
            "systemctl list-units coaxhub-radiod@*.service",
        )?;
        Ok(text
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|u| u.starts_with("coaxhub-radiod@") && u.ends_with(".service") && *u != own)
            .count())
    }
}

/// What came of one connection (for the journal; never the request's bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The peer's UID is not allowed; nothing was read.
    NotPermitted,
    /// No request line within the time.
    TimedOut,
    /// The peer closed before sending a line.
    Closed,
    /// Not a request.
    Refused(Refusal),
    /// `ping` answered.
    Pinged,
    /// `start` or `restart` refused: this many other receiver units are
    /// already running.
    TooMany {
        /// What was asked.
        verb: Verb,
        /// For which receiver.
        id: u32,
        /// The other units running.
        others: usize,
    },
    /// A unit verb ran: whether `systemctl` accepted it.
    Ran {
        /// What was asked.
        verb: Verb,
        /// For which receiver.
        id: u32,
        /// `systemctl` exited 0.
        ok: bool,
    },
}

impl Outcome {
    /// One journal line's worth: fixed words and numbers only.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NotPermitted => "refused: peer not allowed".to_owned(),
            Self::TimedOut => "refused: no request line in time".to_owned(),
            Self::Closed => "peer closed without a request".to_owned(),
            Self::Refused(why) => format!("refused: bad request ({why})"),
            Self::Pinged => "ping ok".to_owned(),
            Self::TooMany { verb, id, others } => {
                format!("{verb} {id}: refused, {others} other receiver units are running")
            }
            Self::Ran { verb, id, ok } => {
                let mut s = format!("{verb} {id}: ");
                s.push_str(if *ok { "ok" } else { "failed" });
                s
            }
        }
    }
}

enum ReadFailure {
    TimedOut,
    Closed,
    Refused(Refusal),
}

/// Reads one line, byte by byte, so nothing after it is consumed: at most
/// [`MAX_REQUEST_BYTES`], before `deadline`.
fn read_line(stream: &UnixStream, deadline: Instant) -> Result<Vec<u8>, ReadFailure> {
    let mut line = Vec::with_capacity(MAX_REQUEST_BYTES);
    let mut reader = stream;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ReadFailure::TimedOut);
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| ReadFailure::Closed)?;
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() => return Err(ReadFailure::Closed),
            Ok(0) => return Err(ReadFailure::Refused(Refusal::NoNewline)),
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(line);
                }
                if line.len() >= MAX_REQUEST_BYTES {
                    return Err(ReadFailure::Refused(Refusal::TooLong));
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(ReadFailure::TimedOut);
            }
            Err(_) => return Err(ReadFailure::Closed),
        }
    }
}

fn reply(stream: &UnixStream, reply: &Reply) {
    let mut writer = stream;
    let _ = writer.write_all(reply.encode().as_bytes());
    let _ = writer.flush();
}

/// Closes our side after the reply and throws away what a refused peer
/// has already sent (up to [`DRAIN_BYTES`], without waiting for more).
fn finish(stream: &UnixStream) {
    let _ = stream.shutdown(Shutdown::Write);
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut reader = stream;
    let mut left = DRAIN_BYTES;
    let mut buf = [0u8; 1024];
    while left > 0 {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}

/// Why a unit verb did not run to the end.
enum Gate {
    /// `start`/`restart` with this many other units running.
    TooMany(usize),
    /// The count or the verb failed: the detail.
    Failed(String),
}

/// `start` and `restart` need room for one more unit; `stop` always goes.
fn gate(runner: &dyn Runner, verb: Verb, id: u32) -> Result<(), Gate> {
    if verb == Verb::Stop {
        return Ok(());
    }
    match runner.busy_others(id) {
        Ok(n) if n >= MAX_RECEIVER_UNITS => Err(Gate::TooMany(n)),
        Ok(_) => Ok(()),
        Err(detail) => Err(Gate::Failed(detail)),
    }
}

/// Serves one connection: the peer check, the request, the action, the
/// reply. `timeout` bounds reading the request.
pub fn handle(
    stream: &UnixStream,
    peer_uid: u32,
    policy: Policy,
    runner: &dyn Runner,
    timeout: Duration,
) -> Outcome {
    let deadline = Instant::now() + timeout;
    if !policy.permits(peer_uid) {
        reply(stream, &Reply::Error(NOT_PERMITTED.into()));
        finish(stream);
        return Outcome::NotPermitted;
    }
    let outcome = match read_line(stream, deadline) {
        Err(ReadFailure::Closed) => return Outcome::Closed,
        Err(ReadFailure::TimedOut) => {
            reply(stream, &Reply::Error("bad request".into()));
            Outcome::TimedOut
        }
        Err(ReadFailure::Refused(why)) => {
            reply(stream, &Reply::Error("bad request".into()));
            Outcome::Refused(why)
        }
        Ok(line) => match Request::parse(&line) {
            Err(why) => {
                reply(stream, &Reply::Error("bad request".into()));
                Outcome::Refused(why)
            }
            Ok(Request::Ping) => {
                reply(stream, &Reply::Ok);
                Outcome::Pinged
            }
            Ok(Request::Unit { verb, id }) => match gate(runner, verb, id)
                .and_then(|()| runner.run(verb, id).map_err(Gate::Failed))
            {
                Err(Gate::TooMany(others)) => {
                    reply(
                        stream,
                        &Reply::Error(format!(
                            "too many receivers: {others} other coaxhub-radiod units are \
                             running (at most {MAX_RECEIVER_UNITS})"
                        )),
                    );
                    Outcome::TooMany { verb, id, others }
                }
                Err(Gate::Failed(detail)) => {
                    reply(stream, &Reply::Error(detail));
                    Outcome::Ran {
                        verb,
                        id,
                        ok: false,
                    }
                }
                Ok(()) => {
                    reply(stream, &Reply::Ok);
                    Outcome::Ran { verb, id, ok: true }
                }
            },
        },
    };
    finish(stream);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_DETAIL_BYTES;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(Verb, u32)>>,
        fail: Option<String>,
        busy: usize,
    }

    impl Runner for Fake {
        fn run(&self, verb: Verb, id: u32) -> Result<(), String> {
            self.calls.lock().expect("lock").push((verb, id));
            self.fail.clone().map_or(Ok(()), Err)
        }

        fn busy_others(&self, _id: u32) -> Result<usize, String> {
            Ok(self.busy)
        }
    }

    const COAXHUB: u32 = 998;
    const POLICY: Policy = Policy {
        coaxhub_uid: Some(COAXHUB),
    };
    const SECOND: Duration = Duration::from_secs(1);

    /// Sends `input` from a client thread, serves the other end, and returns
    /// what the helper decided and everything it answered.
    fn talk(input: &[u8], peer: u32, fake: &Fake, timeout: Duration) -> (Outcome, String) {
        let (client, server) = UnixStream::pair().expect("pair");
        let input = input.to_vec();
        let c = std::thread::spawn(move || {
            let mut client = client;
            // A refused peer may be cut off before it finished: ignore it.
            let _ = client.write_all(&input);
            let mut answer = String::new();
            let _ = client.read_to_string(&mut answer);
            answer
        });
        let outcome = handle(&server, peer, POLICY, fake, timeout);
        drop(server);
        (outcome, c.join().expect("client"))
    }

    #[test]
    fn who_may_talk() {
        assert!(POLICY.permits(0));
        assert!(POLICY.permits(COAXHUB));
        assert!(!POLICY.permits(1));
        assert!(!POLICY.permits(65534));
        let nobody = Policy { coaxhub_uid: None };
        assert!(nobody.permits(0));
        assert!(!nobody.permits(COAXHUB));
    }

    #[test]
    fn uid_from_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      coaxhub-radiod:x:997:997::/var/lib/coaxhub-radiod:/usr/sbin/nologin\n\
                      coaxhub:x:998:998::/var/lib/coaxhub:/usr/sbin/nologin\n";
        assert_eq!(uid_of(passwd, "coaxhub"), Some(998));
        assert_eq!(uid_of(passwd, "coaxhub-radiod"), Some(997));
        assert_eq!(uid_of(passwd, "root"), Some(0));
        assert_eq!(uid_of(passwd, "coax"), None);
        assert_eq!(uid_of("coaxhub:x:notanumber:1\n", "coaxhub"), None);
        assert_eq!(uid_of("coaxhub\n", "coaxhub"), None);
        assert_eq!(uid_of("", "coaxhub"), None);
    }

    #[test]
    fn a_request_runs_and_is_answered() {
        for (line, verb, id) in [
            ("start 11\n", Verb::Start, 11),
            ("restart 12\n", Verb::Restart, 12),
            ("stop 4294967295\n", Verb::Stop, u32::MAX),
        ] {
            let fake = Fake::default();
            let (outcome, answer) = talk(line.as_bytes(), COAXHUB, &fake, SECOND);
            assert_eq!(outcome, Outcome::Ran { verb, id, ok: true });
            assert_eq!(answer, "ok\n");
            assert_eq!(*fake.calls.lock().expect("lock"), [(verb, id)]);
        }
    }

    #[test]
    fn start_and_restart_need_room_stop_does_not() {
        let full = Fake {
            busy: MAX_RECEIVER_UNITS,
            ..Fake::default()
        };
        for (line, verb) in [("start 7\n", Verb::Start), ("restart 7\n", Verb::Restart)] {
            let (outcome, answer) = talk(line.as_bytes(), COAXHUB, &full, SECOND);
            assert_eq!(
                outcome,
                Outcome::TooMany {
                    verb,
                    id: 7,
                    others: MAX_RECEIVER_UNITS
                }
            );
            assert!(
                answer.starts_with("error too many receivers: 16 other"),
                "{answer}"
            );
            assert!(full.calls.lock().expect("lock").is_empty());
        }
        let (outcome, answer) = talk(b"stop 7\n", COAXHUB, &full, SECOND);
        assert_eq!(answer, "ok\n");
        assert!(matches!(outcome, Outcome::Ran { ok: true, .. }));
        // One below the limit still has room.
        let room = Fake {
            busy: MAX_RECEIVER_UNITS - 1,
            ..Fake::default()
        };
        let (_, answer) = talk(b"start 7\n", COAXHUB, &room, SECOND);
        assert_eq!(answer, "ok\n");
        assert_eq!(room.calls.lock().expect("lock").len(), 1);
    }

    #[test]
    fn root_may_ping() {
        let fake = Fake::default();
        let (outcome, answer) = talk(b"ping\n", 0, &fake, SECOND);
        assert_eq!(outcome, Outcome::Pinged);
        assert_eq!(answer, "ok\n");
        assert!(fake.calls.lock().expect("lock").is_empty());
    }

    #[test]
    fn a_failure_reaches_the_peer_on_one_line() {
        let fake = Fake {
            fail: Some(
                "systemctl --no-block start coaxhub-radiod@3.service: Failed to start: Unit not found.\n"
                    .into(),
            ),
            ..Fake::default()
        };
        let (outcome, answer) = talk(b"start 3\n", COAXHUB, &fake, SECOND);
        assert_eq!(
            outcome,
            Outcome::Ran {
                verb: Verb::Start,
                id: 3,
                ok: false
            }
        );
        assert_eq!(
            answer,
            "error systemctl --no-block start coaxhub-radiod@3.service: Failed to start: Unit not found.\n"
        );
        let long = Fake {
            fail: Some("x".repeat(5000)),
            ..Fake::default()
        };
        let (_, answer) = talk(b"stop 3\n", 0, &long, SECOND);
        assert!(answer.len() <= "error ".len() + MAX_DETAIL_BYTES + 1);
    }

    #[test]
    fn hostile_requests_run_nothing() {
        let big = vec![b'a'; 1 << 20];
        let mut nul = b"start 1".to_vec();
        nul.push(0);
        nul.push(b'\n');
        let mut newline_in_the_middle = b"ping\nstart 1\n".to_vec();
        newline_in_the_middle.extend_from_slice(b"stop 1\n");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("enable", b"enable 11\n".to_vec()),
            ("zero", b"start 0\n".to_vec()),
            ("leading zero", b"start 011\n".to_vec()),
            ("two ids", b"start 11 12\n".to_vec()),
            ("unit name", b"start coaxhub-radiod@11\n".to_vec()),
            ("too big", b"start 4294967296\n".to_vec()),
            ("negative", b"start -1\n".to_vec()),
            ("1 MiB", big),
            ("NUL", nul),
            ("non-UTF-8", b"start \xff\xfe\n".to_vec()),
            ("empty line", b"\n".to_vec()),
            ("CRLF", b"start 1\r\n".to_vec()),
            ("ping with an id", b"ping 1\n".to_vec()),
        ];
        for (what, input) in cases {
            let fake = Fake::default();
            let (outcome, answer) = talk(&input, COAXHUB, &fake, SECOND);
            assert!(
                matches!(outcome, Outcome::Refused(_)),
                "{what}: {outcome:?}"
            );
            assert!(
                answer.is_empty() || answer == "error bad request\n",
                "{what}: {answer:?}"
            );
            assert!(fake.calls.lock().expect("lock").is_empty(), "{what}");
        }
        // Only the first line counts: what follows it is never read.
        let fake = Fake::default();
        let (outcome, _) = talk(&newline_in_the_middle, COAXHUB, &fake, SECOND);
        assert_eq!(outcome, Outcome::Pinged);
        assert!(fake.calls.lock().expect("lock").is_empty());
    }

    #[test]
    fn a_1_mib_line_is_answered_not_reset() {
        let fake = Fake::default();
        let big = vec![b'a'; 60_000];
        let (outcome, answer) = talk(&big, COAXHUB, &fake, SECOND);
        assert_eq!(outcome, Outcome::Refused(Refusal::TooLong));
        assert_eq!(answer, "error bad request\n");
    }

    #[test]
    fn another_peer_is_refused_before_it_is_read() {
        let fake = Fake::default();
        let (outcome, answer) = talk(b"start 1\n", 1000, &fake, SECOND);
        assert_eq!(outcome, Outcome::NotPermitted);
        assert_eq!(answer, "error not permitted\n");
        assert!(fake.calls.lock().expect("lock").is_empty());
    }

    #[test]
    fn no_newline_times_out() {
        let fake = Fake::default();
        let (client, server) = UnixStream::pair().expect("pair");
        let mut client = client;
        client.write_all(b"start 1").expect("write");
        let started = Instant::now();
        let outcome = handle(&server, COAXHUB, POLICY, &fake, Duration::from_millis(200));
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(server);
        let mut answer = String::new();
        client.read_to_string(&mut answer).expect("read");
        assert_eq!(answer, "error bad request\n");
        assert!(fake.calls.lock().expect("lock").is_empty());
    }

    #[test]
    fn a_peer_that_leaves_gets_nothing() {
        let fake = Fake::default();
        let (client, server) = UnixStream::pair().expect("pair");
        drop(client);
        assert_eq!(
            handle(&server, COAXHUB, POLICY, &fake, SECOND),
            Outcome::Closed
        );
        let (mut client, server) = UnixStream::pair().expect("pair");
        client.write_all(b"start 1").expect("write");
        client.shutdown(Shutdown::Write).expect("shutdown");
        assert_eq!(
            handle(&server, COAXHUB, POLICY, &fake, SECOND),
            Outcome::Refused(Refusal::NoNewline)
        );
    }

    #[test]
    fn the_journal_line_has_no_request_bytes() {
        assert_eq!(
            Outcome::Refused(Refusal::BadId).describe(),
            "refused: bad request (id is not canonical decimal)"
        );
        assert_eq!(
            Outcome::Ran {
                verb: Verb::Restart,
                id: 5,
                ok: true
            }
            .describe(),
            "restart 5: ok"
        );
        assert_eq!(Outcome::Pinged.describe(), "ping ok");
    }

    #[test]
    fn systemctl_errors_are_reported() {
        let none = Systemctl {
            candidates: vec!["/nonexistent/systemctl".into()],
            ..Systemctl::default()
        };
        let e = none.run(Verb::Start, 1).expect_err("no systemctl");
        assert!(
            e.starts_with("systemctl --no-block start coaxhub-radiod@1.service: "),
            "{e}"
        );
        // `false` exits 1 without a word; `true` exits 0.
        for (path, ok) in [("/bin/false", false), ("/bin/true", true)] {
            if !std::path::Path::new(path).exists() {
                continue;
            }
            let r = Systemctl {
                candidates: vec![path.into()],
                ..Systemctl::default()
            }
            .run(Verb::Stop, 2);
            assert_eq!(r.is_ok(), ok, "{r:?}");
            if let Err(e) = r {
                assert!(
                    e.starts_with("systemctl --no-block stop coaxhub-radiod@2.service: "),
                    "{e}"
                );
            }
        }
        // The other units that are running or starting, from `list-units`.
        let dir = tempfile::tempdir().expect("tmp");
        let script = dir.path().join("systemctl");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat <<'EOF'\ncoaxhub-radiod@1.service loaded active running one\n\
             coaxhub-radiod@3.service loaded activating auto-restart three\n\
             other.service loaded active running not ours\nEOF\n",
        )
        .expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let listing = Systemctl {
            candidates: vec![script],
            ..Systemctl::default()
        };
        assert_eq!(listing.busy_others(9), Ok(2));
        assert_eq!(listing.busy_others(3), Ok(1), "its own unit is not counted");
        assert!(none.busy_others(1).is_err());
        // A systemctl that never answers is killed at the timeout.
        let dir = tempfile::tempdir().expect("tmp");
        let script = dir.path().join("systemctl");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let started = Instant::now();
        let e = Systemctl {
            candidates: vec![script],
            timeout: Duration::from_millis(300),
        }
        .run(Verb::Start, 3)
        .expect_err("killed");
        assert!(e.ends_with(": timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
