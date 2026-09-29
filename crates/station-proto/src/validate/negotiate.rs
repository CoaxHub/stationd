// SPDX-License-Identifier: MIT
//! Version negotiation (station-protocol §3).

use crate::v1::ProtocolVersion;
use crate::{PROTOCOL_VERSION, limits, v1};

/// The peer's protocol version cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unsupported protocol version {}", describe(.peer.as_ref()))]
pub struct Unsupported {
    /// The version the peer offered, if it offered one.
    pub peer: Option<ProtocolVersion>,
}

fn describe(v: Option<&ProtocolVersion>) -> String {
    v.map_or_else(
        || "(none)".to_owned(),
        |v| format!("{}.{}", v.major, v.minor),
    )
}

/// Hub side: the version for this session, from the station's `Hello`.
///
/// The major must be one the hub supports ([`limits::SUPPORTED_MAJORS`]); the
/// session uses the lower of the two minors. The result goes into
/// `Welcome.protocol_version`; on `Err` the hub sends
/// `Goodbye{UNSUPPORTED_VERSION}`.
pub fn negotiate(hello: &v1::Hello) -> Result<ProtocolVersion, Unsupported> {
    negotiate_against(hello, PROTOCOL_VERSION, limits::SUPPORTED_MAJORS)
}

/// [`negotiate`] for a hub at version `ours` accepting `supported_majors`
/// (for tests and conformance suites that simulate other versions).
///
/// For `ours.major` the lower minor wins. For an older supported major
/// (migration window, §3) the station's minor is used as is: the hub keeps
/// the full older major.
pub fn negotiate_against(
    hello: &v1::Hello,
    ours: ProtocolVersion,
    supported_majors: &[u32],
) -> Result<ProtocolVersion, Unsupported> {
    let Some(peer) = hello.protocol_version else {
        return Err(Unsupported { peer: None });
    };
    if !supported_majors.contains(&peer.major) {
        return Err(Unsupported { peer: Some(peer) });
    }
    let minor = if peer.major == ours.major {
        peer.minor.min(ours.minor)
    } else {
        peer.minor
    };
    Ok(ProtocolVersion {
        major: peer.major,
        minor,
    })
}

/// Station side: checks the version the hub chose in `Welcome`.
///
/// It must be this crate's major and a minor no higher than ours (the hub
/// picks the lower minor). The station then uses only features of that minor.
pub fn negotiate_welcome(welcome: &v1::Welcome) -> Result<ProtocolVersion, Unsupported> {
    negotiate_welcome_against(welcome, PROTOCOL_VERSION)
}

/// [`negotiate_welcome`] for a station at version `ours`.
pub fn negotiate_welcome_against(
    welcome: &v1::Welcome,
    ours: ProtocolVersion,
) -> Result<ProtocolVersion, Unsupported> {
    match welcome.protocol_version {
        Some(v) if v.major == ours.major && v.minor <= ours.minor => Ok(v),
        peer => Err(Unsupported { peer }),
    }
}
