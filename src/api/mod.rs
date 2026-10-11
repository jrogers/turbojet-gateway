//! The operator API: an HTTP JSON server in the gateway's process, started when `jetbridge.toml`
//! has `[api]` (see [`Api`]).
//!
//! Every answer is JSON, errors included: [`ApiError`] is `{"error": "..."}` with its status.
//! Handlers reach the gateway and the service through `State`, which holds the service weakly:
//! the service owns the server, so a strong reference from the server's handlers would keep both
//! alive for as long as each other.

use std::io;
use std::net::SocketAddr;
use std::sync::Weak;
use std::time::Duration;

use axum::Json;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{Api, Error};
use crate::service::Service;

/// How long shutdown waits for requests under way to finish before closing their connections.
/// The API's requests are quick: a reload, or a cancel waiting for its commit, takes well under
/// this.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// What every handler can reach.
#[derive(Debug, Clone)]
pub(crate) struct State {
    /// Gone once the service is dropped; see the module documentation.
    #[expect(dead_code, reason = "the endpoints after health reach the gateway and sessions through it")]
    service: Weak<Service>,
}

/// An error answer, the API's only one: `{"error": message}` with `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

/// The API's routes.
fn router(state: State) -> axum::Router {
    axum::Router::new()
        .route("/api/health", get(health))
        .fallback(async |uri: Uri| ApiError::new(StatusCode::NOT_FOUND, format!("no such path: {}", uri.path())))
        .method_not_allowed_fallback(async || ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct Health {
    version: &'static str,
}

/// `GET /api/health`, without authentication: the server is up, and which Jetbridge it is.
async fn health() -> Json<Health> {
    Json(Health { version: env!("CARGO_PKG_VERSION") })
}

/// The API's listener, bound, and its TLS set up: not yet serving. Binding comes before anything
/// else starts, so a port in use or a bad certificate stops nothing half-started.
#[derive(Debug)]
pub(crate) struct Bound {
    config: Api,
    listener: TcpListener,
    #[cfg(feature = "tls")]
    tls: Option<turbojet::tls::ServerTls>,
}

impl Bound {
    /// Binds `config.listen`, or takes `given`, and loads `config.tls`'s certificate and key.
    pub(crate) async fn bind(config: &Api, given: Option<std::net::TcpListener>) -> Result<Self, Error> {
        #[cfg(feature = "tls")]
        let tls = match &config.tls {
            Some(tls) => Some(tls::server(tls).map_err(|e| Error::new(format!("cannot set up the API's TLS: {e}")))?),
            None => None,
        };
        let listen = config.listen;
        let listener = match given {
            Some(listener) => listener
                .set_nonblocking(true)
                .and_then(|()| TcpListener::from_std(listener))
                .map_err(|e| Error::new(format!("cannot listen on the API listener given: {e}")))?,
            None => TcpListener::bind(listen)
                .await
                .map_err(|e| Error::new(format!("cannot bind the API to {listen}: {e}")))?,
        };
        Ok(Self {
            config: config.clone(),
            listener,
            #[cfg(feature = "tls")]
            tls,
        })
    }

    /// Serves the API's routes, with handlers reaching `service`.
    pub(crate) fn serve(self, service: Weak<Service>) -> Server {
        let local_addr = self.listener.local_addr().unwrap_or(self.config.listen);
        let app = router(State { service });
        let (stop, stopped) = oneshot::channel::<()>();
        let stopped = async {
            let _ = stopped.await;
        };
        #[cfg(feature = "tls")]
        let task = match &self.tls {
            Some(tls) => {
                let listener = tls::Listener::new(self.listener, tls.acceptor());
                tokio::spawn(axum::serve(listener, app).with_graceful_shutdown(stopped).into_future())
            }
            None => tokio::spawn(axum::serve(self.listener, app).with_graceful_shutdown(stopped).into_future()),
        };
        #[cfg(not(feature = "tls"))]
        let task = tokio::spawn(axum::serve(self.listener, app).with_graceful_shutdown(stopped).into_future());
        info!(addr = %local_addr, tls = self.config.tls.is_some(), "API listening");
        Server {
            config: self.config,
            local_addr,
            #[cfg(feature = "tls")]
            tls: self.tls,
            stop,
            task,
        }
    }
}

/// The API, serving.
#[derive(Debug)]
pub(crate) struct Server {
    /// As started: a reload changes only the certificate.
    config: Api,
    local_addr: SocketAddr,
    #[cfg(feature = "tls")]
    tls: Option<turbojet::tls::ServerTls>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<io::Result<()>>,
}

impl Server {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting, and waits up to [`SHUTDOWN_WAIT`] for the requests under way.
    pub(crate) async fn shutdown(self) {
        let _ = self.stop.send(());
        let mut task = self.task;
        match tokio::time::timeout(SHUTDOWN_WAIT, &mut task).await {
            Ok(Ok(Ok(()))) => info!("API stopped"),
            Ok(Ok(Err(e))) => warn!("API stopped: {e}"),
            Ok(Err(e)) => warn!("API stopped: {e}"),
            Err(_) => {
                // Stops accepting; a connection still mid-request ends with the runtime, and its
                // handler finds the service gone.
                warn!("API requests still under way after {SHUTDOWN_WAIT:?}: stopping without them");
                task.abort();
            }
        }
    }

    /// Stops at once, without waiting: the service is being dropped.
    pub(crate) fn abort(&self) {
        self.task.abort();
    }

    /// Takes the certificate and key of `config`, the reloaded `[api]`, and returns warnings: for
    /// the changes that wait for a restart, and a certificate that doesn't load (the one in use
    /// is kept).
    pub(crate) fn reload(&self, config: Option<&Api>) -> Vec<String> {
        let Some(config) = config else {
            return vec!["[api] removed: the API keeps serving until a restart".to_string()];
        };
        let mut warnings = Vec::new();
        let unchanged = Api { tls: None, ..config.clone() } == Api { tls: None, ..self.config.clone() }
            && config.tls.is_some() == self.config.tls.is_some();
        if !unchanged {
            warnings.push("[api] changed: only its certificate applies now; the rest at the next start".to_string());
        }
        #[cfg(feature = "tls")]
        if let (Some(server), Some(files)) = (&self.tls, &config.tls) {
            match turbojet::tls::Identity::from_pem_files(&files.cert, &files.key) {
                Ok(identity) => server.set_identity(identity),
                Err(e) => warnings.push(format!("keeping the API's certificate in use: {e}")),
            }
        }
        warnings
    }
}

#[cfg(feature = "tls")]
mod tls {
    use std::io;
    use std::net::SocketAddr;
    use std::time::Duration;

    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinSet;
    use tokio_rustls::server::TlsStream;
    use tracing::{debug, warn};
    use turbojet::tls::{ClientTrust, Identity, ServerTls, TlsAcceptor};

    use crate::config::ApiTls;

    /// How long a client has to finish its TLS handshake.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Handshakes under way at once; connections beyond it are closed. An operator API has a few
    /// clients, so this many at once is a flood, and each costs a task and its buffers.
    const MAX_HANDSHAKES: usize = 64;

    /// Turbojet's reloadable TLS, presenting `files`, without client certificates: the API
    /// authenticates its users itself.
    pub(super) fn server(files: &ApiTls) -> io::Result<ServerTls> {
        ServerTls::new(Identity::from_pem_files(&files.cert, &files.key)?, ClientTrust::None)
    }

    /// Accepts TCP connections and hands axum those whose TLS handshake completes. Handshakes run
    /// on their own tasks, so a slow one doesn't hold up the rest.
    pub(super) struct Listener {
        tcp: TcpListener,
        tls: TlsAcceptor,
        handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
    }

    impl Listener {
        pub(super) fn new(tcp: TcpListener, tls: TlsAcceptor) -> Self {
            Self { tcp, tls, handshakes: JoinSet::new() }
        }
    }

    impl axum::serve::Listener for Listener {
        type Io = TlsStream<TcpStream>;
        type Addr = SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                tokio::select! {
                    accepted = self.tcp.accept() => match accepted {
                        Ok((_, addr)) if self.handshakes.len() >= MAX_HANDSHAKES => {
                            warn!(%addr, "too many API TLS handshakes under way: closing the connection");
                        }
                        Ok((stream, addr)) => {
                            let handshake = self.tls.accept(stream);
                            self.handshakes.spawn(async move {
                                match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
                                    Ok(Ok(stream)) => Some((stream, addr)),
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
                        // Out of file descriptors, say: wait for some to close, as axum does.
                        Err(e) => {
                            warn!("API accept failed: {e}");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    },
                    // Matched in the body: a pattern here that failed would leave the branch off
                    // until the next connection arrives.
                    Some(joined) = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                        if let Ok(Some(done)) = joined {
                            return done;
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
pub(crate) mod tests {
    use super::*;

    /// Unknown paths and methods answer the API's JSON error, not axum's plain text.
    #[tokio::test]
    async fn unknown_paths_and_methods_are_json_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router(State { service: Weak::new() })).into_future());
        let response = get_over(addr, "GET", "/api/nothing").await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert!(response.ends_with(r#"{"error":"no such path: /api/nothing"}"#), "{response}");
        let response = get_over(addr, "POST", "/api/health").await;
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
        assert!(response.ends_with(r#"{"error":"method not allowed"}"#), "{response}");
    }

    /// `method path` sent to `addr` over plain HTTP/1.1, and the whole answer.
    pub(crate) async fn get_over(addr: SocketAddr, method: &str, path: &str) -> String {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        exchange(stream, method, path).await
    }

    /// `method path` sent over `stream`, and the whole answer, the connection closed after it.
    pub(crate) async fn exchange(
        mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        method: &str,
        path: &str,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
            .await
            .expect("no answer")
            .unwrap();
        response
    }
}
