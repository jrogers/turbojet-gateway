//! The operator API: an HTTP JSON server in the gateway's process, started when `jetbridge.toml`
//! has `[api]` (see [`Api`]).
//!
//! Every answer is JSON, errors included: [`ApiError`] is `{"error": "..."}` with its status.
//! Handlers reach the gateway and the service through `State`, which holds the service weakly:
//! the service owns the server, so a strong reference from the server's handlers would keep both
//! alive for as long as each other.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Json;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{Api, Error};
use crate::service::Service;

mod auth;
mod listener;

use auth::Auth;

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
    auth: Arc<Auth>,
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

/// The API's routes. Only health and login are open: every other route goes in `authenticated`,
/// behind [`auth::authenticate`].
fn router(state: State) -> axum::Router {
    let authenticated = axum::Router::new()
        .route("/api/me", get(auth::me))
        .route("/api/logout", post(auth::logout))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth::authenticate));
    axum::Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(auth::login))
        .merge(authenticated)
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
    auth: Arc<Auth>,
}

impl Bound {
    /// Binds `config.listen`, or takes `given`, and loads `config.tls`'s certificate and key and
    /// `config.users`.
    pub(crate) async fn bind(config: &Api, given: Option<std::net::TcpListener>) -> Result<Self, Error> {
        let auth = Arc::new(Auth::load(config).map_err(Error::new)?);
        #[cfg(feature = "tls")]
        let tls = match &config.tls {
            Some(tls) => {
                Some(listener::server(tls).map_err(|e| Error::new(format!("cannot set up the API's TLS: {e}")))?)
            }
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
            auth,
        })
    }

    /// Serves the API's routes, with handlers reaching `service`.
    pub(crate) fn serve(self, service: Weak<Service>) -> Server {
        let local_addr = self.listener.local_addr().unwrap_or(self.config.listen);
        let app = router(State { service, auth: self.auth.clone() });
        let (stop, stopped) = oneshot::channel::<()>();
        #[cfg(feature = "tls")]
        let task = match &self.tls {
            Some(tls) => spawn(listener::Tls::new(self.listener, tls.acceptor()), app, stopped),
            None => spawn(listener::Plain::new(self.listener), app, stopped),
        };
        #[cfg(not(feature = "tls"))]
        let task = spawn(listener::Plain::new(self.listener), app, stopped);
        info!(addr = %local_addr, tls = self.config.tls.is_some(), "API listening");
        Server {
            config: self.config,
            local_addr,
            #[cfg(feature = "tls")]
            tls: self.tls,
            auth: self.auth,
            stop,
            task,
        }
    }
}

/// Serves `app` on `listener` until `stopped` resolves (or its sender is dropped), giving handlers
/// the client's address as `ConnectInfo<SocketAddr>`.
fn spawn<L>(listener: L, app: axum::Router, stopped: oneshot::Receiver<()>) -> JoinHandle<io::Result<()>>
where
    L: axum::serve::Listener<Addr = SocketAddr>,
{
    // Tapped, as axum implements ConnectInfo for its own TcpListener and for any tapped listener.
    let listener = listener.tap_io(|_| ());
    let app = app.into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .into_future(),
    )
}

/// The API, serving.
#[derive(Debug)]
pub(crate) struct Server {
    /// As started: a reload changes only the certificate.
    config: Api,
    local_addr: SocketAddr,
    #[cfg(feature = "tls")]
    tls: Option<turbojet::tls::ServerTls>,
    auth: Arc<Auth>,
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

    /// Rereads the users file the API started with, returning a warning if it doesn't load: the
    /// users in use are kept.
    pub(crate) fn reload_users(&self) -> Option<String> {
        self.auth.reload()
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::ConnectInfo;

    /// The API's routes, and `/test/peer` answering the client's address as handlers see it.
    fn app() -> axum::Router {
        let auth = Auth::new(std::path::PathBuf::new(), crate::auth::Users::default(), false, false);
        router(State { service: Weak::new(), auth: Arc::new(auth) })
            .route("/test/peer", get(async |ConnectInfo(addr): ConnectInfo<SocketAddr>| addr.to_string()))
    }

    /// Serves [`app`] on a plain loopback listener, returning its address and its stop.
    async fn serve_plain() -> (SocketAddr, oneshot::Sender<()>) {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        spawn(listener::Plain::new(tcp), app(), stopped);
        (addr, stop)
    }

    /// Unknown paths and methods answer the API's JSON error, not axum's plain text.
    #[tokio::test]
    async fn unknown_paths_and_methods_are_json_errors() {
        let (addr, _stop) = serve_plain().await;
        let response = request(addr, "GET", "/api/nothing").await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert!(response.ends_with(r#"{"error":"no such path: /api/nothing"}"#), "{response}");
        let response = request(addr, "POST", "/api/health").await;
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
        assert!(response.ends_with(r#"{"error":"method not allowed"}"#), "{response}");
    }

    /// Handlers see the client's address: its end of the connection.
    #[tokio::test]
    async fn handlers_see_the_clients_address() {
        let (addr, _stop) = serve_plain().await;
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let client = stream.local_addr().unwrap();
        assert!(exchange(stream, "GET", "/test/peer").await.ends_with(&client.to_string()));
    }

    #[cfg(feature = "tls")]
    mod tls {
        use super::*;
        use crate::config::ApiTls;

        /// Serves [`app`] over TLS with a new certificate for localhost, returning the address,
        /// the certificate and the stop.
        async fn serve_tls() -> (SocketAddr, String, oneshot::Sender<()>) {
            let dir = tempfile::tempdir().unwrap();
            let (cert, key) = localhost_cert();
            let files = ApiTls { cert: dir.path().join("api.pem"), key: dir.path().join("api.key") };
            std::fs::write(&files.cert, &cert).unwrap();
            std::fs::write(&files.key, key).unwrap();
            let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = tcp.local_addr().unwrap();
            let (stop, stopped) = oneshot::channel();
            spawn(listener::Tls::new(tcp, listener::server(&files).unwrap().acceptor()), app(), stopped);
            (addr, cert, stop)
        }

        #[tokio::test]
        async fn handlers_see_the_clients_address_over_tls() {
            let (addr, cert, _stop) = serve_tls().await;
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let client = tcp.local_addr().unwrap();
            let response = exchange(tls_client(&cert).await.unwrap().connect(tcp).await.unwrap(), "GET", "/test/peer");
            assert!(response.await.ends_with(&client.to_string()));
        }

        /// A client that connects and sends nothing doesn't hold up others' handshakes.
        #[tokio::test]
        async fn a_stalled_connection_doesnt_hold_up_tls_handshakes() {
            let (addr, cert, _stop) = serve_tls().await;
            let _stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Well within the stalled handshake's timeout, so the two can't have run in turn.
            let response = tokio::time::timeout(Duration::from_secs(2), request_tls(addr, &cert, "/api/health"))
                .await
                .expect("held up by the stalled connection")
                .unwrap();
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        }
    }

    /// A self-signed certificate for localhost, as PEM: the certificate and its key.
    #[cfg(feature = "tls")]
    pub(crate) fn localhost_cert() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// A TLS client for localhost trusting only `cert`.
    #[cfg(feature = "tls")]
    pub(crate) struct TlsClient(turbojet::tls::TlsConnector);

    #[cfg(feature = "tls")]
    impl TlsClient {
        pub(crate) async fn connect(
            &self,
            tcp: tokio::net::TcpStream,
        ) -> io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
            self.0.connect("localhost".try_into().expect("a valid name"), tcp).await
        }
    }

    #[cfg(feature = "tls")]
    pub(crate) async fn tls_client(cert: &str) -> io::Result<TlsClient> {
        use turbojet::tls::{ClientTls, Trust};
        Ok(TlsClient(ClientTls::new(Trust::from_pem(cert.as_bytes())?, None)?.connector()))
    }

    /// GET `path` over TLS to `addr`, trusting only `cert`, and the whole answer.
    #[cfg(feature = "tls")]
    pub(crate) async fn request_tls(addr: SocketAddr, cert: &str, path: &str) -> io::Result<String> {
        let tcp = tokio::net::TcpStream::connect(addr).await?;
        let stream = tls_client(cert).await?.connect(tcp).await?;
        Ok(exchange(stream, "GET", path).await)
    }

    /// `method path` sent to `addr` over plain HTTP/1.1, and the whole answer.
    pub(crate) async fn request(addr: SocketAddr, method: &str, path: &str) -> String {
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
