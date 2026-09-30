// SPDX-License-Identifier: GPL-3.0-only
//! radiod group addresses as written in `stationd.toml` (station-protocol
//! §7): `status_group` and `data_group`.
//!
//! Accepted forms, with the port defaulting to radiod's
//! (`DEFAULT_STAT_PORT` 5006 for status, `DEFAULT_RTP_PORT` 5004 for data,
//! `rtp.h`):
//!
//! - `239.77.0.6:5006`, `239.77.0.6`, `[ff02::1]:5006`, `ff02::1`;
//! - a radiod status **name** such as `coaxhub-status.local` or
//!   `coaxhub-status` (optionally with `:port`).
//!
//! A name is turned into an address exactly as radiod does it with its
//! default `dns = no` (`radio.c`, `multicast.c` `make_maddr`): `.local` is
//! appended unless the name already ends with it, and the result is hashed
//! (32-bit FNV-1) into `239.x.y.z`. So `status = coaxhub-status` in
//! radiod's config and `status_group = "coaxhub-status"` in stationd's
//! reach the same group without DNS or mDNS. stationd never resolves names
//! over the network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// radiod's status and command port (`DEFAULT_STAT_PORT`, `rtp.h:11`).
pub const DEFAULT_STATUS_PORT: u16 = 5006;
/// radiod's RTP data port (`DEFAULT_RTP_PORT`, `rtp.h:9`).
pub const DEFAULT_DATA_PORT: u16 = 5004;

/// Longest accepted name (a DNS name).
const MAX_NAME_LEN: usize = 253;

/// Why a group address is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupError {
    /// Nothing was given.
    #[error("the group address is empty")]
    Empty,
    /// The port is not a number in 1..=65535.
    #[error("invalid port in {0:?}")]
    Port(String),
    /// Neither an IP address nor a valid name.
    #[error("{0:?} is neither an IP address nor a host name (letters, digits, '-', '_', '.')")]
    Invalid(String),
}

/// Parses a `status_group`/`data_group` value (see the module docs);
/// `default_port` applies when none is given.
pub fn parse_group(value: &str, default_port: u16) -> Result<SocketAddr, GroupError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GroupError::Empty);
    }
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return if addr.port() == 0 {
            Err(GroupError::Port(value.to_owned()))
        } else {
            Ok(addr)
        };
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, default_port));
    }
    let (name, port) = match value.rsplit_once(':') {
        Some((name, port)) => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| GroupError::Port(value.to_owned()))?;
            (name, port)
        }
        None => (value, default_port),
    };
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        return Err(GroupError::Invalid(value.to_owned()));
    }
    Ok(SocketAddr::new(IpAddr::V4(name_to_group(name)), port))
}

/// The IPv4 group radiod derives from a status or data name when `dns = no`
/// (the default): [`ensure_suffix`] `.local`, then [`make_maddr`].
pub fn name_to_group(name: &str) -> Ipv4Addr {
    make_maddr(&ensure_suffix(name, ".local"))
}

/// radiod's `ensure_suffix()` (`misc.c`): appends `suffix` unless its
/// **first** occurrence in `s` is at the very end.
pub fn ensure_suffix(s: &str, suffix: &str) -> String {
    match s.find(suffix) {
        Some(i) if i + suffix.len() == s.len() => s.to_owned(),
        _ => format!("{s}{suffix}"),
    }
}

/// radiod's `fnv1hash()` (`misc.c`): 32-bit FNV-1 (multiply, then xor).
pub fn fnv1hash(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, &b| {
        hash.wrapping_mul(0x0100_0193) ^ u32::from(b)
    })
}

/// radiod's `make_maddr()` (`multicast.c`): `239.` plus the low 24 bits of
/// [`fnv1hash`], avoiding `239.0.0.0/24` and `239.128.0.0/24` (they share
/// Ethernet multicast MACs with `224.0.0.0/24`).
pub fn make_maddr(name: &str) -> Ipv4Addr {
    if name.is_empty() {
        return Ipv4Addr::UNSPECIFIED;
    }
    let mut addr = (239_u32 << 24) | (fnv1hash(name.as_bytes()) & 0x00ff_ffff);
    if addr & 0x007f_ff00 == 0 {
        addr |= (addr & 0xff) << 8;
    }
    if addr & 0x007f_ff00 == 0 {
        addr |= 0x0010_0000;
    }
    Ipv4Addr::from(addr)
}

/// Where radiod multicasts an audio channel's status copy for data group
/// `data`: the same address on port 5006 (radiod-notes.md §2). For a
/// unicast address (hermetic tests) port 0, so the client binds a free
/// port instead of colliding with the status socket.
pub fn channel_status_addr(data: SocketAddr) -> SocketAddr {
    let port = if data.ip().is_multicast() {
        DEFAULT_STATUS_PORT
    } else {
        0
    };
    SocketAddr::new(data.ip(), port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn numeric_forms() {
        let p = |s| parse_group(s, DEFAULT_STATUS_PORT);
        assert_eq!(p("239.77.0.6:5006"), Ok("239.77.0.6:5006".parse().unwrap()));
        assert_eq!(p(" 239.77.0.6 "), Ok("239.77.0.6:5006".parse().unwrap()));
        assert_eq!(p("127.0.0.1:40000"), Ok("127.0.0.1:40000".parse().unwrap()));
        assert_eq!(p("[ff02::1]:7"), Ok("[ff02::1]:7".parse().unwrap()));
        assert_eq!(p("ff02::1"), Ok("[ff02::1]:5006".parse().unwrap()));
        assert_eq!(
            parse_group("239.77.0.4", DEFAULT_DATA_PORT),
            Ok("239.77.0.4:5004".parse().unwrap())
        );
    }

    #[test]
    fn errors() {
        let p = |s| parse_group(s, DEFAULT_STATUS_PORT);
        assert_eq!(p(""), Err(GroupError::Empty));
        assert_eq!(p("   "), Err(GroupError::Empty));
        assert!(matches!(p("239.77.0.6:0"), Err(GroupError::Port(_))));
        assert!(matches!(p("name:0"), Err(GroupError::Port(_))));
        assert!(matches!(p("name:99999"), Err(GroupError::Port(_))));
        assert!(matches!(p("name:x"), Err(GroupError::Port(_))));
        assert!(matches!(p("a b"), Err(GroupError::Invalid(_))));
        assert!(matches!(p(":5006"), Err(GroupError::Invalid(_))));
        assert!(matches!(p("<script>"), Err(GroupError::Invalid(_))));
        assert!(matches!(
            p(&"a".repeat(MAX_NAME_LEN + 1)),
            Err(GroupError::Invalid(_))
        ));
        assert!(GroupError::Empty.to_string().contains("empty"));
    }

    #[test]
    fn fnv1_reference_values() {
        // FNV-1 (not FNV-1a) 32-bit test vectors.
        assert_eq!(fnv1hash(b""), 0x811c_9dc5);
        assert_eq!(fnv1hash(b"a"), 0x050c_5d7e);
        assert_eq!(fnv1hash(b"foobar"), 0x31f0_b262);
    }

    #[test]
    fn names_hash_like_radiod() {
        assert_eq!(ensure_suffix("hf", ".local"), "hf.local");
        assert_eq!(ensure_suffix("hf.local", ".local"), "hf.local");
        // radiod's strstr() finds the first occurrence only.
        assert_eq!(
            ensure_suffix("a.local.x.local", ".local"),
            "a.local.x.local.local"
        );
        let a = parse_group("coaxhub-status", DEFAULT_STATUS_PORT).unwrap();
        let b = parse_group("coaxhub-status.local:5006", DEFAULT_STATUS_PORT).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.ip(), IpAddr::V4(name_to_group("coaxhub-status")));
        assert!(a.ip().is_multicast());
        assert_eq!(make_maddr(""), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn channel_status_follows_the_data_group() {
        assert_eq!(
            channel_status_addr("239.77.0.4:5004".parse().unwrap()),
            "239.77.0.4:5006".parse().unwrap()
        );
        assert_eq!(
            channel_status_addr("127.0.0.1:0".parse().unwrap()),
            "127.0.0.1:0".parse().unwrap()
        );
    }

    proptest! {
        #[test]
        fn hashed_groups_are_valid_multicast(name in "[a-z0-9._-]{1,40}") {
            let ip = make_maddr(&name);
            prop_assert!(ip.is_multicast());
            prop_assert_eq!(ip.octets()[0], 239);
            let middle = u32::from(ip) & 0x007f_ff00;
            prop_assert_ne!(middle, 0);
        }

        #[test]
        fn parse_never_panics(s in ".{0,80}") {
            let _ = parse_group(&s, DEFAULT_STATUS_PORT);
        }
    }
}
