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
//! - [`config`]: `stationd.toml` (§7) and the owner's [`config::LocalPolicy`].
//! - [`tls`]: TLS 1.3-only mTLS trusting only `gateway_ca`.
//! - [`daemon`]: the reconnect loop and the in-process API ([`spawn`],
//!   [`DaemonHandle`], [`ConnectionState`]).
//! - [`dispatch`]: request handling (every request answered exactly once).
//! - [`outbound`]: the prioritised, bounded outbound queue (§6.3).
//! - [`status`]: `Hello`, `StationStatus` and host metrics.
//! - [`cli`], [`logging`]: command line and `tracing` setup.

pub mod cli;
pub mod config;
pub mod daemon;
pub mod dispatch;
pub mod logging;
pub mod outbound;
mod session;
pub mod status;
pub mod tls;

pub use config::{Config, LocalPolicy};
pub use daemon::{ConnectionState, DaemonHandle, RunError, RunOptions, StopReason, run, spawn};
