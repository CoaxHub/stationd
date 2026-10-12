// SPDX-License-Identifier: GPL-3.0-only
//! `stationd`, the `CoaxHub` station daemon.
//!
//! It dials **out** to the home hub's gateway over HTTP/2 + TLS 1.3 with
//! mutual TLS, opens one `StationGateway.Connect` stream, says `Hello`, and
//! then answers the hub's requests (station-protocol §2–§6): it reports its
//! SDRs, runs the receivers the hub configures (one DSP
//! [`station_backend::Backend`] each) and serves channels, waterfalls and
//! measurements on them. It reconnects with jittered exponential backoff
//! after any disconnect (§2.2) and never needs action on the station when
//! the hub restarts (NFR-5); its receivers run whether connected or not.
//!
//! Library layout (the binary in `main.rs` is a thin CLI over it):
//!
//! - [`config`]: built-in settings, or a development `stationd.toml` (§7);
//!   [`flavour`]: development versus packaged builds.
//! - [`hardware`]: the SDRs on the USB buses and the package's SDR table;
//!   [`profile`]: the SDR profiles; [`receivers`]: the applied receiver set
//!   (`receivers.json`), each receiver's unit and backend, and their states.
//! - [`state`]: the state directory (atomic writes, strict permissions);
//!   [`credentials`]: where the tunnel's credentials come from (static or
//!   enrolled), re-read before every connect attempt.
//! - [`enroll`]: `Enrollment.Enroll` against the `CoaxHub` root only;
//!   [`station_api`]: `StationControl` (`GetAssignment`,
//!   `RenewCertificate`) with the station certificate; [`renew`]: when to
//!   renew, and the renewal task.
//! - [`tls`]: TLS 1.3-only (m)TLS trusting one explicit bundle.
//! - [`daemon`]: the reconnect loop and the in-process API ([`spawn`],
//!   [`DaemonHandle`], [`ConnectionState`]).
//! - [`dispatch`]: request handling (every request answered exactly once).
//! - [`outbound`]: the prioritised, bounded outbound queue (§6.3).
//! - [`measure`]: the noise floor (§4.1): plans per receiver, the idle
//!   schedule, the reduction.
//! - [`status`]: `Hello`, `StationStatus` and host metrics.
//! - [`local_status`]: the runtime status file and `stationd status`;
//!   [`doctor`]: `stationd doctor`; [`backend_config`]: a receiver's radiod
//!   config from its profile and `receivers.json`.
//! - [`cli`], [`logging`]: command line and `tracing` setup.

pub mod backend_config;
pub mod cli;
pub mod config;
pub mod credentials;
pub mod daemon;
pub mod dispatch;
pub mod doctor;
pub mod enroll;
pub mod flavour;
pub mod hardware;
pub mod local_status;
pub mod logging;
pub mod measure;
pub mod outbound;
pub mod profile;
pub mod receivers;
pub mod renew;
mod rpc;
mod session;
pub mod state;
pub mod station_api;
pub mod status;
pub mod tls;

pub use config::Config;
pub use daemon::{
    ConnectionState, DaemonHandle, Observed, RunError, RunOptions, StopReason, run, spawn,
    spawn_with,
};
