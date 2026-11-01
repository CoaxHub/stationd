// SPDX-License-Identifier: GPL-3.0-only
//! `coaxhub-helper`: started by `coaxhub-helper@.service` for one connection
//! on `coaxhub-helper.socket`; the connection is its standard input and
//! output. It serves that one connection and exits.
//!
//! Every request goes to the journal (standard error): the peer's UID and the
//! outcome, never the request's bytes.

use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use coaxhub_helper::protocol::REQUEST_TIMEOUT;
use coaxhub_helper::server::{self, Policy, STATIOND_USER, Systemctl};

/// The most of `/etc/passwd` read to find `coaxhub`.
const PASSWD_LIMIT: u64 = 1 << 20;

fn main() -> ExitCode {
    // The connection systemd accepted is standard input.
    let stream = match std::io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => UnixStream::from(fd),
        Err(e) => {
            eprintln!("coaxhub-helper: cannot use standard input: {e}");
            return ExitCode::FAILURE;
        }
    };
    let peer = match rustix::net::sockopt::socket_peercred(&stream) {
        Ok(cred) => cred.uid.as_raw(),
        Err(e) => {
            eprintln!(
                "coaxhub-helper: standard input is not a socket ({e}); this program runs from \
                 coaxhub-helper.socket"
            );
            return ExitCode::FAILURE;
        }
    };
    let mut passwd = String::new();
    let coaxhub_uid = std::fs::File::open("/etc/passwd")
        .and_then(|f| f.take(PASSWD_LIMIT).read_to_string(&mut passwd))
        .ok()
        .and_then(|_| server::uid_of(&passwd, STATIOND_USER));
    if coaxhub_uid.is_none() {
        eprintln!("coaxhub-helper: no user {STATIOND_USER} in /etc/passwd: only root may ask");
    }
    let outcome = server::handle(
        &stream,
        peer,
        Policy { coaxhub_uid },
        &Systemctl::default(),
        REQUEST_TIMEOUT,
    );
    eprintln!("coaxhub-helper: peer uid {peer}: {}", outcome.describe());
    ExitCode::SUCCESS
}
