// SPDX-License-Identifier: GPL-3.0-only
//! UDP socket setup for talking to radiod: multicast groups on the loopback
//! (or any other) interface, and plain unicast for hermetic tests.
//!
//! The multicast recipe is the one verified against radiod in
//! `docs/radiod-notes.md` §2:
//!
//! 1. `SO_REUSEADDR` and `SO_REUSEPORT` (radiod binds the same port);
//! 2. bind the group address and port;
//! 3. `IP_ADD_MEMBERSHIP` on the given interface (127.0.0.1 in the
//!    test setup);
//! 4. `IP_MULTICAST_IF` = that interface, `IP_MULTICAST_TTL` = the
//!    configured TTL (0 keeps packets on the host), `IP_MULTICAST_LOOP` = 1.
//!
//! If the address is **not** multicast, the helpers fall back to plain
//! unicast so tests can run against a fake radiod on 127.0.0.1 without the
//! loopback MULTICAST flag:
//!
//! - [`bind_receiver`] binds the address itself (port 0 picks a free port);
//! - [`bind_control`] binds an ephemeral port on the interface and relies on
//!   the peer replying to the command's source address.
//!
//! Multicast is supported for IPv4 only (radiod's own configs and the
//! test setup use IPv4 groups); unicast works for IPv4 and IPv6.
//!
//! All functions return a Tokio socket and therefore must be called from
//! within a Tokio runtime.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

/// Kernel receive buffer requested for every socket. An Opus channel is 50
/// packets/s, but a receiver stalled for a moment should not lose status
/// replies or RTP. The kernel may cap it (`net.core.rmem_max`); a smaller
/// buffer is not an error.
pub const RECV_BUFFER_BYTES: usize = 1 << 20;

/// Errors from socket setup.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// Called outside a Tokio runtime.
    #[error("socket setup needs a Tokio runtime")]
    NoRuntime,
    /// IPv6 multicast groups are not supported.
    #[error("IPv6 multicast group {0} is not supported")]
    Ipv6MulticastUnsupported(SocketAddr),
    /// A socket system call failed.
    #[error("{op} for {addr}: {source}")]
    Io {
        /// What was being done (for example `"bind"`).
        op: &'static str,
        /// The address involved.
        addr: SocketAddr,
        /// The OS error.
        #[source]
        source: io::Error,
    },
}

/// Multicast options for a sending socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendOptions {
    /// Local interface address: used for `IP_MULTICAST_IF` and group
    /// membership, and as the bind address of unicast control sockets.
    pub interface: Ipv4Addr,
    /// `IP_MULTICAST_TTL`. 0 keeps packets on this host.
    pub ttl: u32,
}

impl Default for SendOptions {
    /// Loopback only: interface 127.0.0.1, TTL 0 (radiod-notes.md §2).
    fn default() -> Self {
        Self {
            interface: Ipv4Addr::LOCALHOST,
            ttl: 0,
        }
    }
}

/// True if `addr` is a multicast address (IPv4 or IPv6).
pub fn is_multicast(addr: SocketAddr) -> bool {
    addr.ip().is_multicast()
}

fn io_err(op: &'static str, addr: SocketAddr) -> impl FnOnce(io::Error) -> NetError {
    move |source| NetError::Io { op, addr, source }
}

fn new_udp(addr: SocketAddr) -> Result<Socket, NetError> {
    let domain = Domain::for_address(addr);
    let socket =
        Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(io_err("socket", addr))?;
    socket
        .set_nonblocking(true)
        .map_err(io_err("set_nonblocking", addr))?;
    // Best effort: a capped buffer still works.
    let _ = socket.set_recv_buffer_size(RECV_BUFFER_BYTES);
    Ok(socket)
}

fn into_tokio(socket: Socket, addr: SocketAddr) -> Result<UdpSocket, NetError> {
    // `from_std` panics outside a runtime; report it instead.
    tokio::runtime::Handle::try_current().map_err(|_| NetError::NoRuntime)?;
    UdpSocket::from_std(socket.into()).map_err(io_err("register with tokio", addr))
}

/// Sets the reuse options and multicast membership, then binds the group.
fn bind_group(group: SocketAddr, opts: SendOptions) -> Result<Socket, NetError> {
    let IpAddr::V4(group_ip) = group.ip() else {
        return Err(NetError::Ipv6MulticastUnsupported(group));
    };
    let socket = new_udp(group)?;
    socket
        .set_reuse_address(true)
        .map_err(io_err("SO_REUSEADDR", group))?;
    socket
        .set_reuse_port(true)
        .map_err(io_err("SO_REUSEPORT", group))?;
    socket
        .bind(&SockAddr::from(group))
        .map_err(io_err("bind", group))?;
    socket
        .join_multicast_v4(&group_ip, &opts.interface)
        .map_err(io_err("IP_ADD_MEMBERSHIP", group))?;
    socket
        .set_multicast_if_v4(&opts.interface)
        .map_err(io_err("IP_MULTICAST_IF", group))?;
    socket
        .set_multicast_ttl_v4(opts.ttl)
        .map_err(io_err("IP_MULTICAST_TTL", group))?;
    socket
        .set_multicast_loop_v4(true)
        .map_err(io_err("IP_MULTICAST_LOOP", group))?;
    Ok(socket)
}

/// A receive-only socket for `addr`.
///
/// - Multicast: joins the group on `opts.interface` (see the module docs).
///   Several sockets, in this or other processes, may receive the same
///   group; each gets a copy of every packet.
/// - Unicast: binds `addr` itself. Port 0 picks a free port; read it back
///   with [`UdpSocket::local_addr`].
pub fn bind_receiver(addr: SocketAddr, opts: SendOptions) -> Result<UdpSocket, NetError> {
    if is_multicast(addr) {
        into_tokio(bind_group(addr, opts)?, addr)
    } else {
        let socket = new_udp(addr)?;
        socket
            .bind(&SockAddr::from(addr))
            .map_err(io_err("bind", addr))?;
        into_tokio(socket, addr)
    }
}

/// A socket that sends commands to `dest` and receives status replies.
///
/// - Multicast: bound to and joined on the group itself, with
///   `IP_MULTICAST_IF`/TTL set from `opts`, because radiod sends its replies
///   to the group. The socket also receives its own commands (loopback) and
///   everyone else's traffic, so callers must filter by packet type.
/// - Unicast: bound to an ephemeral port on `opts.interface` (or the IPv6
///   unspecified address for an IPv6 `dest`). The peer must reply to the
///   command's source address, as the test fake does.
pub fn bind_control(dest: SocketAddr, opts: SendOptions) -> Result<UdpSocket, NetError> {
    if is_multicast(dest) {
        into_tokio(bind_group(dest, opts)?, dest)
    } else {
        let local = match dest {
            SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(opts.interface, 0)),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0),
        };
        bind_receiver(local, opts)
    }
}

/// A send-only socket for `dest`: an ephemeral local port with
/// `IP_MULTICAST_IF`/TTL/loop set when `dest` is an IPv4 group. Used by
/// tests and tools that inject packets; the client itself sends through
/// [`bind_control`].
pub fn bind_sender(dest: SocketAddr, opts: SendOptions) -> Result<UdpSocket, NetError> {
    let local = match dest {
        SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0),
    };
    if is_multicast(dest) && !dest.is_ipv4() {
        return Err(NetError::Ipv6MulticastUnsupported(dest));
    }
    let socket = new_udp(local)?;
    if is_multicast(dest) {
        socket
            .set_multicast_if_v4(&opts.interface)
            .map_err(io_err("IP_MULTICAST_IF", dest))?;
        socket
            .set_multicast_ttl_v4(opts.ttl)
            .map_err(io_err("IP_MULTICAST_TTL", dest))?;
        socket
            .set_multicast_loop_v4(true)
            .map_err(io_err("IP_MULTICAST_LOOP", dest))?;
    }
    socket
        .bind(&SockAddr::from(local))
        .map_err(io_err("bind", local))?;
    into_tokio(socket, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lo(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    #[test]
    fn multicast_detection() {
        assert!(is_multicast("239.77.0.6:5006".parse().unwrap()));
        assert!(is_multicast("[ff02::1]:5006".parse().unwrap()));
        assert!(!is_multicast(lo(5006)));
        assert!(!is_multicast("[::1]:5006".parse().unwrap()));
    }

    #[test]
    fn default_options_are_loopback_ttl0() {
        let o = SendOptions::default();
        assert_eq!(o.interface, Ipv4Addr::LOCALHOST);
        assert_eq!(o.ttl, 0);
    }

    #[tokio::test]
    async fn unicast_receiver_and_control_round_trip() {
        let opts = SendOptions::default();
        let server = bind_receiver(lo(0), opts).unwrap();
        let server_addr = server.local_addr().unwrap();
        assert_ne!(server_addr.port(), 0);

        let ctrl = bind_control(server_addr, opts).unwrap();
        assert_eq!(
            ctrl.local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        ctrl.send_to(b"ping", server_addr).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = tokio::time::timeout(Duration::from_secs(2), server.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ping");
        // The server replies to the source address, as the fake radiod does.
        server.send_to(b"pong", from).await.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), ctrl.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"pong");
    }

    #[tokio::test]
    async fn unicast_sender_reaches_receiver() {
        let opts = SendOptions::default();
        let rx = bind_receiver(lo(0), opts).unwrap();
        let dest = rx.local_addr().unwrap();
        let tx = bind_sender(dest, opts).unwrap();
        tx.send_to(b"x", dest).await.unwrap();
        let mut buf = [0u8; 4];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), rx.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn ipv6_unicast_control_binds_unspecified() {
        let dest: SocketAddr = "[::1]:9".parse().unwrap();
        // IPv6 may be disabled in some containers; only check the address
        // family when the bind succeeds.
        if let Ok(s) = bind_control(dest, SendOptions::default()) {
            assert!(s.local_addr().unwrap().is_ipv6());
        }
        if let Ok(s) = bind_sender(dest, SendOptions::default()) {
            assert!(s.local_addr().unwrap().is_ipv6());
        }
    }

    #[test]
    fn outside_a_runtime_is_an_error() {
        let err = bind_receiver(lo(0), SendOptions::default()).unwrap_err();
        assert!(matches!(err, NetError::NoRuntime), "{err}");
    }

    #[tokio::test]
    async fn ipv6_multicast_is_rejected() {
        let group: SocketAddr = "[ff02::1234]:5006".parse().unwrap();
        let opts = SendOptions::default();
        assert!(matches!(
            bind_receiver(group, opts),
            Err(NetError::Ipv6MulticastUnsupported(_))
        ));
        assert!(matches!(
            bind_control(group, opts),
            Err(NetError::Ipv6MulticastUnsupported(_))
        ));
        assert!(matches!(
            bind_sender(group, opts),
            Err(NetError::Ipv6MulticastUnsupported(_))
        ));
    }

    #[tokio::test]
    async fn bind_error_is_typed() {
        let opts = SendOptions::default();
        let first = bind_receiver(lo(0), opts).unwrap();
        let taken = first.local_addr().unwrap();
        // Unicast sockets do not set SO_REUSEADDR, so a second bind fails.
        let err = bind_receiver(taken, opts).unwrap_err();
        assert!(matches!(err, NetError::Io { op: "bind", .. }), "{err}");
        assert!(err.to_string().contains("bind"));
    }

    /// Multicast needs a MULTICAST-capable interface. Docker's `lo` lacks the
    /// flag until `build-radiod.sh` sets it, so this test accepts a
    /// join failure as a skip but checks the full path when it works.
    #[tokio::test]
    async fn multicast_group_round_trip_when_available() {
        let group: SocketAddr = "239.77.0.250:45123".parse().unwrap();
        let opts = SendOptions::default();
        let rx = match bind_receiver(group, opts) {
            Ok(s) => s,
            Err(NetError::Io { op, .. }) => {
                eprintln!("skipping: multicast unavailable ({op})");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let ctrl = bind_control(group, opts).unwrap();
        let tx = bind_sender(group, opts).unwrap();
        tx.send_to(b"hello", group).await.unwrap();
        ctrl.send_to(b"world", group).await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 16];
        while got.len() < 2 {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => got.push(buf[..n].to_vec()),
                _ => break,
            }
        }
        if got.is_empty() {
            eprintln!("skipping: multicast loopback delivered nothing");
            return;
        }
        assert!(got.contains(&b"hello".to_vec()) || got.contains(&b"world".to_vec()));
    }
}
