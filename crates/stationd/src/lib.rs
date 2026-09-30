// SPDX-License-Identifier: GPL-3.0-only
//! `stationd`, the `CoaxHub` station daemon.
//!
//! It dials **out** to the home hub's gateway over HTTP/2 + TLS 1.3 with
//! mutual TLS, opens one `StationGateway.Connect` stream, says `Hello`, and
//! then answers the hub's requests with a DSP [`station_backend::Backend`]
//! (station-protocol §2–§6). It reconnects with jittered exponential backoff
//! after any disconnect (§2.2) and never needs action on the station when
//! the hub restarts (NFR-5).
//!
//! Library layout (the binary in `main.rs` is a thin CLI over it):
//!
//! - [`config`]: `stationd.toml` (§7) and the owner's [`config::LocalPolicy`];
//!   [`flavour`]: development versus packaged builds; [`profile`]: SDR
//!   profiles and the fixed radiod groups.
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
//! - [`status`]: `Hello`, `StationStatus` and host metrics.
//! - [`local_status`]: the runtime status file and `stationd status`;
//!   [`doctor`]: `stationd doctor`; [`backend_config`]: radiod's config
//!   from the SDR profile.
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
pub mod local_status;
pub mod logging;
pub mod outbound;
pub mod profile;
pub mod renew;
mod rpc;
mod session;
pub mod state;
pub mod station_api;
pub mod status;
pub mod tls;

pub use config::{Config, LocalPolicy};
pub use daemon::{
    ConnectionState, DaemonHandle, Observed, RunError, RunOptions, StopReason, run, spawn,
    spawn_with,
};
