// SPDX-License-Identifier: GPL-3.0-only
//! Each receiver's backend process as a unit: started, restarted, stopped
//! and watched (station-protocol §9).
//!
//! - [`Helper`]: `coaxhub-radiod@<receiver_id>.service` under systemd, as a
//!   packaged station runs it. `start`, `restart` and `stop` go to
//!   `coaxhub-helper` on its socket (docs/packaging.md §4): `stationd` is
//!   not root and asks nobody's permission, so a root helper that does
//!   exactly those three things for those units does them. The reads are
//!   [`Systemd`]'s: `systemctl show` reads the unit's state, which needs no
//!   authorization, and the unit's cgroup its CPU time.
//! - [`Systemd`]: the same units through `systemctl` alone (`start`,
//!   `restart` and `stop` need the caller to be allowed by systemd), and the
//!   reads [`Helper`] uses.
//! - [`FakeUnits`]: in memory, for tests and development stations with the
//!   fake backend.
//! - [`ExternalUnits`]: a radiod of the developer's own, always running;
//!   `stationd` starts nothing.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use coaxhub_helper::protocol::{self as helper, Reply, Request, Verb};
use station_proto::limits;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::dispatch::truncate_utf8;

/// A receiver's unit as `stationd` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitState {
    /// Running. `invocation` names this run: a restart changes it.
    Active {
        /// systemd's `InvocationID`.
        invocation: String,
    },
    /// Starting, restarting, or waiting to restart after a failure.
    Activating,
    /// Not running, and not failed.
    Inactive,
    /// Failed: why (at most 256 bytes).
    Failed {
        /// From the unit's result.
        detail: String,
    },
}

/// Starts, restarts, stops and watches the receivers' units.
#[async_trait::async_trait]
pub trait Units: Send + Sync + fmt::Debug {
    /// Starts receiver `id`'s unit (nothing if it runs). `Err` says why it
    /// could not be asked to.
    async fn start(&self, id: u32) -> Result<(), String>;
    /// Restarts it (starts it if it doesn't run).
    async fn restart(&self, id: u32) -> Result<(), String>;
    /// Stops it.
    async fn stop(&self, id: u32) -> Result<(), String>;
    /// Its state now.
    async fn state(&self, id: u32) -> UnitState;
    /// The CPU time its processes used so far, microseconds; `None` when
    /// unknown.
    fn cpu_usage_usec(&self, id: u32) -> Option<u64> {
        let _ = id;
        None
    }
}

/// The unit of receiver `id`.
pub fn unit_name(id: u32) -> String {
    format!("coaxhub-radiod@{id}.service")
}

/// Where the cgroup v2 hierarchy is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// How long a `systemctl` call may take.
pub const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// The receivers' units under systemd.
#[derive(Debug, Clone)]
pub struct Systemd {
    /// The `systemctl` to run.
    pub systemctl: PathBuf,
    /// Where the cgroup hierarchy is ([`CGROUP_ROOT`]).
    pub cgroup_root: PathBuf,
}

impl Default for Systemd {
    fn default() -> Self {
        Self {
            systemctl: PathBuf::from("systemctl"),
            cgroup_root: PathBuf::from(CGROUP_ROOT),
        }
    }
}

impl Systemd {
    async fn systemctl(&self, args: &[&str]) -> Result<String, String> {
        let mut command = tokio::process::Command::new(&self.systemctl);
        command
            .arg("--no-ask-password")
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(SYSTEMCTL_TIMEOUT, command.output())
            .await
            .map_err(|_| format!("systemctl {} timed out", args.join(" ")))?
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(truncate_utf8(
                &format!("systemctl {}: {}", args.join(" "), stderr.trim()),
                limits::MAX_DETAIL_BYTES,
            ))
        }
    }

    async fn verb(&self, verb: &str, id: u32) -> Result<(), String> {
        self.systemctl(&["--no-block", verb, &unit_name(id)])
            .await
            .map(drop)
    }

    /// One property of a unit (`systemctl show --value`), trimmed.
    pub async fn property(&self, unit: &str, property: &str) -> Result<String, String> {
        self.systemctl(&["show", "--value", &format!("--property={property}"), unit])
            .await
            .map(|v| v.trim().to_owned())
    }
}

/// How long a call to the helper may take: more than the helper's worst
/// case, which `RuntimeMaxSec=15` in its unit ends
/// ([`helper::RUNTIME_MAX`]), so that stationd never gives up on a request
/// the helper still carries out and reports the receiver `failed` meanwhile.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(helper::RUNTIME_MAX.as_secs() + 5);

/// Why a call to the helper failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    /// In words for the owner.
    pub text: String,
    /// The helper refused this user (the socket's mode, or its peer check),
    /// as opposed to not answering.
    pub denied: bool,
}

impl CallError {
    fn other(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            denied: false,
        }
    }
}

/// The receivers' units under systemd, started, restarted and stopped by
/// `coaxhub-helper` and read like [`Systemd`] does.
#[derive(Debug, Clone)]
pub struct Helper {
    /// The helper's socket ([`helper::SOCKET_PATH`]).
    pub socket: PathBuf,
    /// Where the reads go.
    pub systemd: Systemd,
    /// How long a call may take.
    pub timeout: Duration,
}

impl Default for Helper {
    fn default() -> Self {
        Self {
            socket: PathBuf::from(helper::SOCKET_PATH),
            systemd: Systemd::default(),
            timeout: HELPER_TIMEOUT,
        }
    }
}

impl Helper {
    /// A helper at `socket` (tests, `doctor`).
    pub fn at(socket: &Path) -> Self {
        Self {
            socket: socket.to_owned(),
            ..Self::default()
        }
    }

    /// Sends `request` and reads the answer. `Err` says why, in words for
    /// the owner: it reaches `receiver_state` and the station's page.
    pub async fn call(&self, request: Request) -> Result<(), String> {
        self.send(request).await.map_err(|e| e.text)
    }

    /// [`Helper::call`] with the failure's kind.
    pub async fn send(&self, request: Request) -> Result<(), CallError> {
        let work = async {
            let mut stream = tokio::net::UnixStream::connect(&self.socket)
                .await
                .map_err(|e| CallError {
                    text: format!(
                        "cannot reach coaxhub-helper ({}): {e}; run sudo stationd doctor",
                        self.socket.display()
                    ),
                    denied: e.kind() == std::io::ErrorKind::PermissionDenied,
                })?;
            stream
                .write_all(request.encode().as_bytes())
                .await
                .map_err(|e| {
                    CallError::other(format!("coaxhub-helper ({}): {e}", self.socket.display()))
                })?;
            let mut answer = Vec::new();
            // One short line; never more than a reply can be.
            (&mut stream)
                .take(1024)
                .read_to_end(&mut answer)
                .await
                .map_err(|e| {
                    CallError::other(format!("coaxhub-helper ({}): {e}", self.socket.display()))
                })?;
            Ok::<_, CallError>(answer)
        };
        let answer = tokio::time::timeout(self.timeout, work)
            .await
            .map_err(|_| CallError::other("coaxhub-helper timed out"))??;
        if answer.is_empty() {
            return Err(CallError::other(
                "coaxhub-helper closed the connection without an answer",
            ));
        }
        match Reply::parse(&String::from_utf8_lossy(&answer)) {
            Some(Reply::Ok) => Ok(()),
            Some(Reply::Error(detail)) => Err(CallError {
                denied: detail == helper::NOT_PERMITTED,
                text: truncate_utf8(&detail, limits::MAX_DETAIL_BYTES),
            }),
            None => Err(CallError::other(
                "coaxhub-helper answered something unexpected",
            )),
        }
    }

    /// Asks the helper to answer `ok` (it runs nothing).
    pub async fn ping(&self) -> Result<(), CallError> {
        self.send(Request::Ping).await
    }

    async fn unit(&self, verb: Verb, id: u32) -> Result<(), String> {
        self.call(Request::Unit { verb, id }).await
    }
}

/// The state of a unit from `systemctl show`'s `key=value` lines.
pub fn parse_show(text: &str, id: u32) -> UnitState {
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or("")
            .trim()
    };
    let failed = || {
        let mut detail = format!("{} failed ({}", unit_name(id), get("Result"));
        let status = get("ExecMainStatus");
        if !status.is_empty() && status != "0" {
            let _ = write!(detail, ", exit status {status}");
        }
        detail.push(')');
        UnitState::Failed {
            detail: truncate_utf8(&detail, limits::MAX_DETAIL_BYTES),
        }
    };
    match get("ActiveState") {
        "active" | "reloading" => UnitState::Active {
            invocation: get("InvocationID").to_owned(),
        },
        "activating" | "deactivating" => UnitState::Activating,
        "failed" => failed(),
        _ if get("SubState") == "auto-restart" => UnitState::Activating,
        _ if !matches!(get("Result"), "" | "success") => failed(),
        _ => UnitState::Inactive,
    }
}

#[async_trait::async_trait]
impl Units for Systemd {
    async fn start(&self, id: u32) -> Result<(), String> {
        self.verb("start", id).await
    }

    async fn restart(&self, id: u32) -> Result<(), String> {
        self.verb("restart", id).await
    }

    async fn stop(&self, id: u32) -> Result<(), String> {
        self.verb("stop", id).await
    }

    async fn state(&self, id: u32) -> UnitState {
        match self
            .systemctl(&[
                "show",
                "--property=ActiveState,SubState,Result,ExecMainStatus,InvocationID",
                &unit_name(id),
            ])
            .await
        {
            Ok(text) => parse_show(&text, id),
            Err(detail) => UnitState::Failed { detail },
        }
    }

    fn cpu_usage_usec(&self, id: u32) -> Option<u64> {
        // systemd escapes the template's '-' in the slice name.
        let path = self
            .cgroup_root
            .join("system.slice")
            .join("system-coaxhub\\x2dradiod.slice")
            .join(unit_name(id))
            .join("cpu.stat");
        parse_cpu_stat(&std::fs::read_to_string(path).ok()?)
    }
}

#[async_trait::async_trait]
impl Units for Helper {
    async fn start(&self, id: u32) -> Result<(), String> {
        self.unit(Verb::Start, id).await
    }

    async fn restart(&self, id: u32) -> Result<(), String> {
        self.unit(Verb::Restart, id).await
    }

    async fn stop(&self, id: u32) -> Result<(), String> {
        self.unit(Verb::Stop, id).await
    }

    async fn state(&self, id: u32) -> UnitState {
        self.systemd.state(id).await
    }

    fn cpu_usage_usec(&self, id: u32) -> Option<u64> {
        self.systemd.cpu_usage_usec(id)
    }
}

/// `usage_usec` of a cgroup v2 `cpu.stat`.
pub fn parse_cpu_stat(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

/// What [`FakeUnits`] was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitCall {
    /// `start`.
    Start(u32),
    /// `restart`.
    Restart(u32),
    /// `stop`.
    Stop(u32),
}

#[derive(Debug, Default)]
struct FakeState {
    units: HashMap<u32, UnitState>,
    runs: u64,
    failing: HashMap<u32, String>,
    calls: Vec<UnitCall>,
    held: bool,
}

/// Units in memory: a start makes a unit active at once, unless the test
/// made it fail ([`FakeUnits::fail`]) or holds starts
/// ([`FakeUnits::hold_starts`]). Clones share the units.
#[derive(Debug, Clone, Default)]
pub struct FakeUnits {
    state: Arc<Mutex<FakeState>>,
    released: Arc<tokio::sync::Notify>,
}

impl FakeUnits {
    fn locked(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes receiver `id`'s unit fail with `detail` now and on every start
    /// (`None`: start normally again).
    pub fn fail(&self, id: u32, detail: Option<&str>) {
        let mut s = self.locked();
        match detail {
            Some(d) => {
                s.failing.insert(id, d.to_owned());
                s.units.insert(
                    id,
                    UnitState::Failed {
                        detail: d.to_owned(),
                    },
                );
            }
            None => {
                s.failing.remove(&id);
            }
        }
    }

    /// Restarts receiver `id`'s unit by itself (as `Restart=on-failure`
    /// does after a crash): a new run.
    pub fn crash(&self, id: u32) {
        let mut s = self.locked();
        Self::run(&mut s, id);
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<UnitCall> {
        self.locked().calls.clone()
    }

    /// Holds every start, once asked for, until [`FakeUnits::release_starts`]:
    /// a slow `systemctl start`.
    pub fn hold_starts(&self) {
        self.locked().held = true;
    }

    /// Lets the held starts and the next ones go on.
    pub fn release_starts(&self) {
        self.locked().held = false;
        self.released.notify_waiters();
    }

    fn run(s: &mut FakeState, id: u32) {
        let next = if let Some(d) = s.failing.get(&id) {
            UnitState::Failed { detail: d.clone() }
        } else {
            s.runs += 1;
            UnitState::Active {
                invocation: format!("fake-{}", s.runs),
            }
        };
        s.units.insert(id, next);
    }
}

#[async_trait::async_trait]
impl Units for FakeUnits {
    async fn start(&self, id: u32) -> Result<(), String> {
        self.locked().calls.push(UnitCall::Start(id));
        loop {
            let released = self.released.notified();
            if !self.locked().held {
                break;
            }
            released.await;
        }
        let mut s = self.locked();
        if !matches!(s.units.get(&id), Some(UnitState::Active { .. })) {
            Self::run(&mut s, id);
        }
        Ok(())
    }

    async fn restart(&self, id: u32) -> Result<(), String> {
        let mut s = self.locked();
        s.calls.push(UnitCall::Restart(id));
        Self::run(&mut s, id);
        Ok(())
    }

    async fn stop(&self, id: u32) -> Result<(), String> {
        let mut s = self.locked();
        s.calls.push(UnitCall::Stop(id));
        s.units.insert(id, UnitState::Inactive);
        Ok(())
    }

    async fn state(&self, id: u32) -> UnitState {
        self.locked()
            .units
            .get(&id)
            .cloned()
            .unwrap_or(UnitState::Inactive)
    }
}

/// A radiod the developer runs: always active, never started or stopped by
/// `stationd`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExternalUnits;

#[async_trait::async_trait]
impl Units for ExternalUnits {
    async fn start(&self, _id: u32) -> Result<(), String> {
        Ok(())
    }

    async fn restart(&self, _id: u32) -> Result<(), String> {
        Ok(())
    }

    async fn stop(&self, _id: u32) -> Result<(), String> {
        Ok(())
    }

    async fn state(&self, _id: u32) -> UnitState {
        UnitState::Active {
            invocation: "external".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemctl_show() {
        let show = |active: &str, sub: &str, result: &str, status: &str| {
            format!(
                "ActiveState={active}\nSubState={sub}\nResult={result}\nExecMainStatus={status}\n\
                 InvocationID=0123abcd\n"
            )
        };
        assert_eq!(
            parse_show(&show("active", "running", "success", "0"), 3),
            UnitState::Active {
                invocation: "0123abcd".into()
            }
        );
        assert_eq!(
            parse_show(&show("activating", "start-pre", "success", "0"), 3),
            UnitState::Activating
        );
        assert_eq!(
            parse_show(&show("inactive", "auto-restart", "exit-code", "1"), 3),
            UnitState::Activating
        );
        assert_eq!(
            parse_show(&show("failed", "failed", "exit-code", "1"), 3),
            UnitState::Failed {
                detail: "coaxhub-radiod@3.service failed (exit-code, exit status 1)".into()
            }
        );
        assert_eq!(
            parse_show(&show("inactive", "dead", "signal", "0"), 3),
            UnitState::Failed {
                detail: "coaxhub-radiod@3.service failed (signal)".into()
            }
        );
        assert_eq!(
            parse_show(&show("inactive", "dead", "success", "0"), 3),
            UnitState::Inactive
        );
        // A unit whose ExecCondition= said no is skipped: inactive, result success.
        assert_eq!(
            parse_show(&show("inactive", "dead", "success", "0"), 3),
            UnitState::Inactive
        );
        assert_eq!(parse_show("", 3), UnitState::Inactive);
        assert_eq!(unit_name(12), "coaxhub-radiod@12.service");
    }

    #[test]
    fn cpu_stat() {
        assert_eq!(
            parse_cpu_stat("usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\n"),
            Some(123_456)
        );
        assert_eq!(parse_cpu_stat("nr_periods 0\n"), None);
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp
            .path()
            .join("system.slice/system-coaxhub\\x2dradiod.slice/coaxhub-radiod@4.service");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("cpu.stat"), "usage_usec 42\n").expect("write");
        let units = Systemd {
            cgroup_root: tmp.path().to_owned(),
            ..Systemd::default()
        };
        assert_eq!(units.cpu_usage_usec(4), Some(42));
        assert_eq!(units.cpu_usage_usec(5), None);
    }

    #[tokio::test]
    async fn systemctl_errors_are_reported() {
        let units = Systemd {
            systemctl: PathBuf::from("/nonexistent/systemctl"),
            ..Systemd::default()
        };
        let e = units.start(1).await.expect_err("no systemctl");
        assert!(e.contains("cannot run systemctl"), "{e}");
        assert!(matches!(units.state(1).await, UnitState::Failed { .. }));
        // A systemctl that refuses (the unit is missing).
        let units = Systemd {
            systemctl: PathBuf::from("false"),
            ..Systemd::default()
        };
        let e = units.restart(2).await.expect_err("refused");
        assert!(
            e.starts_with("systemctl --no-block restart coaxhub-radiod@2.service"),
            "{e}"
        );
        assert!(units.stop(2).await.is_err());
    }

    /// Serves a socket in `dir`: `answer` gets each request line and returns
    /// the bytes to send back (`None`: hang). Returns the socket's path and
    /// the request lines seen.
    fn fake_helper(
        dir: &Path,
        answer: impl Fn(&str) -> Option<Vec<u8>> + Send + 'static,
    ) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        use std::io::{BufRead as _, BufReader, Write as _};
        let path = dir.join("helper.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).expect("line");
                log.lock().expect("lock").push(line.clone());
                match answer(&line) {
                    Some(bytes) => {
                        let _ = stream.write_all(&bytes);
                    }
                    None => std::thread::sleep(Duration::from_secs(5)),
                }
            }
        });
        (path, seen)
    }

    #[tokio::test]
    async fn the_helper_is_asked_for_start_restart_and_stop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (socket, seen) = fake_helper(tmp.path(), |line| {
            Some(if line == "restart 9\n" {
                b"error systemctl --no-block restart coaxhub-radiod@9.service: Unit not found.\n"
                    .to_vec()
            } else {
                b"ok\n".to_vec()
            })
        });
        let units = Helper::at(&socket);
        units.start(3).await.expect("start");
        units.stop(3).await.expect("stop");
        units.ping().await.expect("ping");
        let e = units.restart(9).await.expect_err("refused");
        assert_eq!(
            e,
            "systemctl --no-block restart coaxhub-radiod@9.service: Unit not found."
        );
        assert_eq!(
            *seen.lock().expect("lock"),
            ["start 3\n", "stop 3\n", "ping\n", "restart 9\n"]
        );
    }

    #[test]
    fn timeouts_agree_with_the_helper() {
        // stationd waits longer than the helper can possibly take.
        assert!(HELPER_TIMEOUT > helper::RUNTIME_MAX);
        assert_eq!(helper::MAX_RECEIVER_UNITS, limits::MAX_RECEIVERS);
    }

    #[tokio::test]
    async fn a_refusal_of_the_user_is_told_from_no_answer() {
        let tmp = tempfile::tempdir().expect("tmp");
        let (socket, _) = fake_helper(tmp.path(), |_| Some(b"error not permitted\n".to_vec()));
        let e = Helper::at(&socket).ping().await.expect_err("refused");
        assert!(e.denied, "{e:?}");
        let e = Helper::at(&tmp.path().join("none.sock"))
            .ping()
            .await
            .expect_err("no socket");
        assert!(!e.denied, "{e:?}");
    }

    #[tokio::test]
    async fn the_helper_not_answering_is_reported() {
        let tmp = tempfile::tempdir().expect("tmp");
        // No socket at all.
        let missing = tmp.path().join("none.sock");
        let e = Helper::at(&missing).start(1).await.expect_err("no socket");
        assert!(e.starts_with("cannot reach coaxhub-helper ("), "{e}");
        assert!(e.contains("none.sock"), "{e}");
        assert!(e.ends_with("; run sudo stationd doctor"), "{e}");
        // Closed without a word.
        let dir = tmp.path().join("a");
        std::fs::create_dir(&dir).expect("mkdir");
        let (socket, _) = fake_helper(&dir, |_| Some(Vec::new()));
        let e = Helper::at(&socket).stop(1).await.expect_err("closed");
        assert_eq!(e, "coaxhub-helper closed the connection without an answer");
        // Something else.
        let dir = tmp.path().join("b");
        std::fs::create_dir(&dir).expect("mkdir");
        let (socket, _) = fake_helper(&dir, |_| Some(b"maybe\n".to_vec()));
        let e = Helper::at(&socket).stop(1).await.expect_err("odd");
        assert_eq!(e, "coaxhub-helper answered something unexpected");
        // A long detail is cut to the protocol's limit.
        let dir = tmp.path().join("c");
        std::fs::create_dir(&dir).expect("mkdir");
        let (socket, _) = fake_helper(&dir, |_| {
            Some(format!("error {}\n", "x".repeat(900)).into_bytes())
        });
        let e = Helper::at(&socket).stop(1).await.expect_err("long");
        assert!(e.len() <= limits::MAX_DETAIL_BYTES, "{}", e.len());
        // Hangs.
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).expect("mkdir");
        let (socket, _) = fake_helper(&dir, |_| None);
        let units = Helper {
            timeout: Duration::from_millis(200),
            ..Helper::at(&socket)
        };
        let e = units.start(1).await.expect_err("timeout");
        assert_eq!(e, "coaxhub-helper timed out");
    }

    #[tokio::test]
    async fn fake_units() {
        let u = FakeUnits::default();
        assert_eq!(u.state(1).await, UnitState::Inactive);
        u.start(1).await.expect("start");
        let UnitState::Active { invocation } = u.state(1).await else {
            panic!("active expected");
        };
        u.start(1).await.expect("start again");
        assert_eq!(
            u.state(1).await,
            UnitState::Active {
                invocation: invocation.clone()
            }
        );
        u.restart(1).await.expect("restart");
        assert_ne!(u.state(1).await, UnitState::Active { invocation });
        u.fail(1, Some("radiod: no device"));
        assert_eq!(
            u.state(1).await,
            UnitState::Failed {
                detail: "radiod: no device".into()
            }
        );
        u.restart(1).await.expect("restart");
        assert!(matches!(u.state(1).await, UnitState::Failed { .. }));
        u.fail(1, None);
        u.crash(1);
        assert!(matches!(u.state(1).await, UnitState::Active { .. }));
        u.stop(1).await.expect("stop");
        assert_eq!(u.state(1).await, UnitState::Inactive);
        assert_eq!(
            u.calls(),
            [
                UnitCall::Start(1),
                UnitCall::Start(1),
                UnitCall::Restart(1),
                UnitCall::Restart(1),
                UnitCall::Stop(1)
            ]
        );
        assert!(u.cpu_usage_usec(1).is_none());
        let e = ExternalUnits;
        e.start(1).await.expect("start");
        e.restart(1).await.expect("restart");
        e.stop(1).await.expect("stop");
        assert!(matches!(e.state(1).await, UnitState::Active { .. }));
    }
}
