// SPDX-License-Identifier: MIT
//! Conformance suite for `CoaxHub` station implementations
//! (`coaxhub.station.v1`, station-protocol §10).
//!
//! MIT, like the protocol, so that anyone writing a station — the
//! GPL `stationd` or an alternative — can check it against the same
//! scripted hub. It depends only on `station-proto`.
//!
//! - [`hub::FakeHub`]: a real mTLS gRPC `StationGateway` server on loopback,
//!   scripted from the test through [`hub::StationConn`], validating every
//!   station message with the §5 validator and recording
//!   everything.
//! - [`pki::TestPki`]: throwaway certificates generated in memory.
//! - [`sut::StationUnderTest`]: how scenarios start and stop the station;
//!   [`sut::SimulatedSdr`]: the SDRs it sees, plugged and unplugged by the
//!   scenarios.
//! - [`scenarios`]: named conformance scenarios ([`scenarios::ALL`]).
//!
//! ```no_run
//! # async fn demo(station: &mut dyn station_conformance::StationUnderTest) {
//! use station_conformance::scenarios::{self, ScenarioConfig};
//! let cfg = ScenarioConfig::default();
//! for s in scenarios::ALL {
//!     (s.run)(station, &cfg).await.unwrap_or_else(|e| panic!("{}: {e}", s.name));
//! }
//! # }
//! ```

pub mod error;
pub mod hub;
mod io;
pub mod pki;
pub mod scenarios;
pub mod sut;

pub use error::{BoxError, ConformanceError, Result};
pub use hub::{FakeHub, HubTls, Record, RecordEvent, StationConn, TlsVersions};
pub use io::ConnInfo;
pub use pki::TestPki;
pub use scenarios::{Scenario, ScenarioConfig};
pub use sut::{Exit, SimulatedSdr, StationUnderTest, Target};
