// SPDX-License-Identifier: MIT
//! The fake hub's server-side connection: a TLS stream the script can kill
//! (abrupt close, like a crashed hub or a cut link) or freeze (a peer that
//! stops answering, even HTTP/2 pings).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use tonic::transport::server::Connected;

/// What the hub knows about one TLS connection; available to the gRPC
/// handler through the request extensions.
#[derive(Debug, Clone)]
pub struct ConnInfo {
    /// Connection number, from 1, unique per [`crate::hub::FakeHub`].
    pub id: u64,
    /// The station's address.
    pub remote_addr: Option<SocketAddr>,
    /// The client certificate chain the station presented.
    pub peer_certs: Vec<CertificateDer<'static>>,
    pub(crate) kill: CancellationToken,
    pub(crate) frozen: Arc<AtomicBool>,
}

impl ConnInfo {
    /// Closes the TCP connection abruptly.
    pub fn kill(&self) {
        self.kill.cancel();
    }

    /// Stops reading and writing (the peer sees silence) until killed.
    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }
}

pub(crate) struct KillableIo {
    inner: TlsStream<TcpStream>,
    killed: Pin<Box<WaitForCancellationFutureOwned>>,
    dead: bool,
    info: ConnInfo,
}

impl KillableIo {
    pub(crate) fn new(inner: TlsStream<TcpStream>, info: ConnInfo) -> Self {
        Self {
            inner,
            killed: Box::pin(info.kill.clone().cancelled_owned()),
            dead: false,
            info,
        }
    }

    /// `Some(Err)` once killed, `Some(Pending)` while frozen, `None` to
    /// proceed. Polling the kill future registers the waker, so a pending
    /// frozen or idle stream still wakes up when killed.
    fn gate<T>(&mut self, cx: &mut Context<'_>) -> Option<Poll<io::Result<T>>> {
        if self.dead || self.killed.as_mut().poll(cx).is_ready() {
            self.dead = true;
            return Some(Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection killed by the fake hub",
            ))));
        }
        if self.info.frozen.load(Ordering::SeqCst) {
            return Some(Poll::Pending);
        }
        None
    }
}

impl Connected for KillableIo {
    type ConnectInfo = ConnInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

impl AsyncRead for KillableIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(p) = self.gate(cx) {
            return p;
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for KillableIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(p) = self.gate(cx) {
            return p;
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(p) = self.gate(cx) {
            return p;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(p) = self.gate(cx) {
            return p;
        }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
