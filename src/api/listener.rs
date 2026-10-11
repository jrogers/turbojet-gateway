//! The API's listeners, plain and TLS, each holding at most [`MAX_CONNECTIONS`] connections.
//! Beyond that, accepting waits until one closes: the new connections queue in the kernel's
//! backlog. There is no idle timeout, so an idle connection keeps its slot.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::warn;

/// Connections open at once, TLS handshakes under way included. An operator API has a few users
/// and a console or two; this many at once is a flood, and each costs a task and its buffers.
pub(super) const MAX_CONNECTIONS: usize = 256;

/// A connection, holding its slot until it's dropped.
#[derive(Debug)]
pub(super) struct Conn<S> {
    stream: S,
    _slot: OwnedSemaphorePermit,
}

/// Plain TCP, capped.
#[derive(Debug)]
pub(super) struct Plain {
    tcp: TcpListener,
    slots: Arc<Semaphore>,
}

impl Plain {
    pub(super) fn new(tcp: TcpListener) -> Self {
        Self { tcp, slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)) }
    }
}

impl axum::serve::Listener for Plain {
    type Io = Conn<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, addr, slot) = accept(&self.tcp, &self.slots).await;
        (Conn { stream, _slot: slot }, addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// The next connection, once a slot is free. Errors for one connection are skipped at once;
/// others (out of file descriptors, say) wait a second for some to close, as axum's own do.
async fn accept(tcp: &TcpListener, slots: &Arc<Semaphore>) -> (TcpStream, SocketAddr, OwnedSemaphorePermit) {
    let slot = slots.clone().acquire_owned().await.expect("the semaphore is never closed");
    loop {
        match tcp.accept().await {
            Ok((stream, addr)) => return (stream, addr, slot),
            Err(e) if is_connection_error(&e) => {}
            Err(e) => {
                warn!("API accept failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// An error about the one connection being accepted, not the listener.
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
    )
}

impl<S: AsyncRead + Unpin> AsyncRead for Conn<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Conn<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(feature = "tls")]
pub(super) use tls::{Tls, server};

#[cfg(feature = "tls")]
mod tls {
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{OwnedSemaphorePermit, Semaphore};
    use tokio::task::JoinSet;
    use tokio_rustls::server::TlsStream;
    use tracing::debug;
    use turbojet::tls::{ClientTrust, Identity, ServerTls, TlsAcceptor};

    use super::{Conn, MAX_CONNECTIONS, accept};
    use crate::config::ApiTls;

    /// How long a client has to finish its TLS handshake.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Turbojet's reloadable TLS, presenting `files`, without client certificates: the API
    /// authenticates its users itself.
    pub(in crate::api) fn server(files: &ApiTls) -> io::Result<ServerTls> {
        ServerTls::new(Identity::from_pem_files(&files.cert, &files.key)?, ClientTrust::None)
    }

    type Handshaken = Option<(TlsStream<TcpStream>, SocketAddr, OwnedSemaphorePermit)>;

    /// TLS over TCP, capped: hands axum the connections whose handshake completes. Handshakes
    /// run on their own tasks, so a slow one doesn't hold up the rest.
    pub(in crate::api) struct Tls {
        tcp: TcpListener,
        tls: TlsAcceptor,
        slots: Arc<Semaphore>,
        handshakes: JoinSet<Handshaken>,
    }

    impl Tls {
        pub(in crate::api) fn new(tcp: TcpListener, tls: TlsAcceptor) -> Self {
            Self { tcp, tls, slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)), handshakes: JoinSet::new() }
        }
    }

    impl axum::serve::Listener for Tls {
        type Io = Conn<TlsStream<TcpStream>>;
        type Addr = SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                tokio::select! {
                    (stream, addr, slot) = accept(&self.tcp, &self.slots) => {
                        let handshake = self.tls.accept(stream);
                        self.handshakes.spawn(async move {
                            match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
                                Ok(Ok(stream)) => Some((stream, addr, slot)),
                                Ok(Err(e)) => {
                                    debug!(%addr, "API TLS handshake failed: {e}");
                                    None
                                }
                                Err(_) => {
                                    debug!(%addr, "API TLS handshake timed out");
                                    None
                                }
                            }
                        });
                    }
                    // Matched in the body: a pattern here that failed would leave the branch off
                    // until the next connection arrives.
                    Some(joined) = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                        if let Ok(Some((stream, addr, slot))) = joined {
                            return (Conn { stream, _slot: slot }, addr);
                        }
                    }
                }
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            self.tcp.local_addr()
        }
    }
}
