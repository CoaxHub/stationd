// SPDX-License-Identifier: MIT
//! The `CoaxHub` station protocol, `coaxhub.station.v1`, for both sides of the
//! tunnel: the hub gateway (untrusted input) and `stationd`.
//!
//! This crate is MIT. It is what a hub and a station must agree on, so everything
//! shared lives here:
//!
//! - [`v1`]: prost messages and enums generated from `proto`, plus the
//!   tonic `StationGateway`, `Enrollment` and `StationControl` clients and
//!   servers.
//! - [`PROTOCOL_VERSION`] and [`limits`]: every number from
//!   `docs/station-protocol.md` (§2.1, §2.2, §4, §5, §6.3, §7, §8).
//! - [`validate`]: the hub-side validation of §5 (stateless checks plus the
//!   stateful [`validate::Session`]), version negotiation (§3), strikes and
//!   the ingress quota; and the rules of a receiver set (§4.2), shared by
//!   the station, the hub and the control plane.
//! - [`backoff`]: the station's reconnection policy (§2.2).
//! - [`identity`]: the station ID from the mTLS client certificate (§8).
//!
//! Nothing here performs I/O or reads a clock: callers pass `now`, and random
//! numbers come from an injected source, so every rule is deterministic under
//! test.

#![warn(missing_docs)]

pub mod backoff;
pub mod identity;
pub mod limits;
pub mod validate;

/// Generated code for the protobuf package `coaxhub.station.v1`.
///
/// Messages and enums, plus the tonic modules `station_gateway_client`,
/// `station_gateway_server`, `enrollment_client`, `enrollment_server`,
/// `station_control_client` and `station_control_server`. Doc comments come
/// from the `.proto` files.
///
/// Enum-typed fields are `i32` (prost's representation); use the generated
/// `TryFrom<i32>` impls, or [`validate`], to reject unknown values.
#[allow(
    missing_docs,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    rustdoc::invalid_html_tags,
    rustdoc::bare_urls,
    rustdoc::broken_intra_doc_links
)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/coaxhub.station.v1.rs"));
}

/// The protocol version this crate implements: `coaxhub.station.v1`, 2.1
/// (station-protocol §3). Major 2: a station has receivers. Minor 1 added
/// `Capabilities.cw_pitch_hz`.
pub const PROTOCOL_VERSION: v1::ProtocolVersion = v1::ProtocolVersion { major: 2, minor: 1 };
