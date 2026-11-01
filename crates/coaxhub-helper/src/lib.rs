// SPDX-License-Identifier: GPL-3.0-only
//! `coaxhub-helper`: the one privileged door of a `CoaxHub` station.
//!
//! `stationd` runs as the unprivileged user `coaxhub`, so it cannot ask
//! systemd to start the receivers' units. This helper runs as root, one
//! short-lived instance per connection on the socket `coaxhub-helper.socket`
//! (`coaxhub-helper@.service`, sandboxed), and does exactly three things for
//! the `coaxhub` user and root: `start`, `restart` and `stop` of
//! `coaxhub-radiod@<id>.service`, plus `ping`. What keeps it to those is this
//! crate's small parser and the fixed `systemctl` argument list, not the
//! sandbox: root on systemd's private socket is root. It reads no file
//! `stationd` can write (docs/packaging.md §4).
//!
//! - [`protocol`]: the one-line request and the reply, shared with `stationd`.
//! - [`server`]: reading a request from the socket, the peer check, running
//!   `systemctl`.

pub mod protocol;
pub mod server;
