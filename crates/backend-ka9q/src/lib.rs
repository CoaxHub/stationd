// SPDX-License-Identifier: GPL-3.0-only
//! ka9q-radio (`radiod`) backend for stationd.
//!
//! stationd drives radiod as a separate local process through its TLV
//! control/status protocol and consumes its RTP output. Nothing is
//! linked with radiod. See docs/station-protocol.md §9.
//!
//! - [`backend`]: [`backend::Ka9qBackend`], the `station_backend::Backend`
//!   stationd uses for `[backend] kind = "ka9q"`.
//! - [`client`]: the async radiod client it is built on.
//! - [`command`], [`status`], [`tlv`], [`tags`]: the TLV control protocol.
//! - [`rtp`], [`spectrum`], [`net`]: RTP, spectrum rows and sockets.

pub mod backend;
pub mod client;
#[cfg(test)]
mod codegen;
pub mod command;
pub mod net;
pub mod rtp;
pub mod spectrum;
pub mod status;
pub mod tags;
pub mod tlv;
