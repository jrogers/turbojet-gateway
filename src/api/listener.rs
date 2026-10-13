//! The API's listeners, plain and TLS, each holding at most [`MAX_CONNECTIONS`] connections.
//! Beyond that, accepting waits until one closes: the new connections queue in the kernel's
//! backlog. A connection that moves no bytes for [`IDLE`] is closed, so idle ones give their
//! slots back.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};
use tracing::warn;

/// Connections open at once, TLS handshakes under way included. An operator API has a few users
/// and a console or two; this many at once is a flood, and each costs a task and its buffers.
pub(super) const MAX_CONNECTIONS: usize = 256;

/// How long a connection may move no bytes before it's closed. axum sets no read or keep-alive
/// timeout, so without this [`MAX_CONNECTIONS`] silent connections would hold every slot and keep
/// operators out. Live sockets ping more often than this; a request whose handler moves no bytes
/// for this long (no reload or fsync should) is cut off with its connection.
pub(super) const IDLE: Duration = Duration::from_secs(60);

/// A connection, holding its slot until it's dropped, and failing once it has been idle for
/// `idle`.
#[derive(Debug)]
pub(super) struct Conn<S> {
    stream: S,
    _slot: OwnedSemaphorePermit,
    idle: Duration,
    deadline: Pin<Box<Sleep>>,
}

impl<S> Conn<S> {
    fn new(stream: S, slot: OwnedSemaphorePermit, idle: Duration) -> Self {
        Self { stream, _slot: slot, idle, deadline: Box::pin(tokio::time::sleep(idle)) }
    }

    /// Pushes the deadline back after bytes moved, or fails once it has passed while waiting.
    fn check<T>(&mut self, cx: &mut Context<'_>, polled: Poll<io::Result<T>>, moved: bool) -> Poll<io::Result<T>> {
        match polled {
            Poll::Ready(Ok(_)) if moved => {
                let idle = self.idle;
                self.deadline.as_mut().reset(Instant::now() + idle);
                polled
            }
            Poll::Pending if self.deadline.as_mut().poll(cx).is_ready() => {
                Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "API connection idle")))
            }
            _ => polled,
        }
    }
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
        (Conn::new(stream, slot, IDLE), addr)
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
        let this = self.get_mut();
        let filled = buf.filled().len();
        let polled = Pin::new(&mut this.stream).poll_read(cx, buf);
        let moved = buf.filled().len() > filled;
        this.check(cx, polled, moved)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Conn<S> {
    // A peer that stops reading stalls writes; the deadline frees its slot too.
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.stream).poll_write(cx, buf);
        let moved = matches!(polled, Poll::Ready(Ok(n)) if n > 0);
        this.check(cx, polled, moved)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.stream).poll_write_vectored(cx, bufs);
        let moved = matches!(polled, Poll::Ready(Ok(n)) if n > 0);
        this.check(cx, polled, moved)
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

    use super::{Conn, IDLE, MAX_CONNECTIONS, accept};
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
                            return (Conn::new(stream, slot, IDLE), addr);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const IDLE_SHORT: Duration = Duration::from_millis(200);

    fn conn() -> (Conn<tokio::io::DuplexStream>, tokio::io::DuplexStream) {
        let (server, client) = tokio::io::duplex(64);
        let slot = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        (Conn::new(server, slot, IDLE_SHORT), client)
    }

    /// A client that sends nothing has its connection's reads fail once the deadline passes.
    #[tokio::test]
    async fn an_idle_connection_times_out() {
        let (mut conn, _client) = conn();
        let started = std::time::Instant::now();
        let read = tokio::time::timeout(IDLE_SHORT * 10, conn.read(&mut [0; 8])).await.unwrap();
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= IDLE_SHORT);
    }

    /// A client sending a byte every quarter deadline, for several deadlines, keeps its connection
    /// open, with the server waiting on each; so do the server's own writes while it reads nothing.
    #[tokio::test]
    async fn an_active_connection_stays_open() {
        let (mut conn, mut client) = conn();
        for _ in 0..12 {
            assert_eq!(read_after_quarter(&mut conn, &mut client).await, 1);
        }
        for _ in 0..12 {
            tokio::time::sleep(IDLE_SHORT / 4).await;
            conn.write_all(b"y").await.unwrap();
            assert_eq!(client.read(&mut [0; 8]).await.unwrap(), 1);
        }
        assert_eq!(read_after_quarter(&mut conn, &mut client).await, 1);
    }

    /// The server's read, waiting while the client sends a byte a quarter deadline later.
    async fn read_after_quarter(
        conn: &mut Conn<tokio::io::DuplexStream>,
        client: &mut tokio::io::DuplexStream,
    ) -> usize {
        let mut buf = [0; 8];
        let (read, ()) = tokio::join!(conn.read(&mut buf), async {
            tokio::time::sleep(IDLE_SHORT / 4).await;
            client.write_all(b"x").await.unwrap();
        });
        read.unwrap()
    }
}
