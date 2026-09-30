// SPDX-License-Identifier: MIT
//! The error type of the conformance harness.

use std::time::Duration;

use station_proto::validate::ViolationKind;

/// A boxed error from a station under test.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Why a scenario (or a harness step) failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConformanceError {
    /// Nothing happened in time.
    #[error("timed out after {after:?} waiting for {what}")]
    Timeout {
        /// What was awaited.
        what: String,
        /// How long.
        after: Duration,
    },
    /// The station closed its stream.
    #[error("the station closed the stream while waiting for {what}")]
    StreamClosed {
        /// What was awaited.
        what: String,
    },
    /// The stream failed (transport error, reset).
    #[error("the station's stream failed while waiting for {what}: {status}")]
    StreamError {
        /// What was awaited.
        what: String,
        /// The gRPC status.
        status: String,
    },
    /// A message other than the expected one arrived.
    #[error("expected {expected}, got {got}")]
    Unexpected {
        /// What the script expected.
        expected: String,
        /// What arrived (Debug of the message).
        got: String,
    },
    /// The hub-side validator (`station_proto::validate`, station-protocol
    /// §5) found a protocol violation.
    #[error("protocol violation by the station: {kind} (message: {message})")]
    Violation {
        /// The violation.
        kind: ViolationKind,
        /// Debug of the offending message.
        message: String,
    },
    /// A scenario assertion failed.
    #[error("{0}")]
    Assertion(String),
    /// The fake hub itself failed (bind, TLS setup, script misuse).
    #[error("fake hub: {0}")]
    Hub(String),
    /// The station under test failed to start or stop.
    #[error("station under test: {0}")]
    Station(String),
    /// Test certificates could not be generated.
    #[error("test PKI: {0}")]
    Pki(String),
}

/// Shorthand for results of this crate.
pub type Result<T, E = ConformanceError> = std::result::Result<T, E>;

/// Fails with [`ConformanceError::Assertion`] unless `cond` holds.
pub fn ensure(cond: bool, message: impl FnOnce() -> String) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(ConformanceError::Assertion(message()))
    }
}
