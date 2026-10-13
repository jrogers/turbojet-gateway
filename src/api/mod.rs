//! The operator API: an HTTP JSON server in the gateway's process, started when `jetbridge.toml`
//! has `[api]` (see [`Api`]).
//!
//! Every answer is JSON, errors included: [`ApiError`] is `{"error": "..."}` with its status.
//! Handlers reach the gateway and the service through `State`, which holds the service weakly:
//! the service owns the server, so a strong reference from the server's handlers would keep both
//! alive for as long as each other.
//!
//! Mutations run through `audited`, which writes their audit line before they run (see
//! [`crate::audit`]).

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Json;
use axum::extract::Query;
use axum::extract::rejection::QueryRejection;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::serve::ListenerExt;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::audit::{Audit, Entry};
use crate::auth::Role;
use crate::config::{Api, Error};
use crate::service::Service;

mod auth;
mod listener;
mod live;
mod messages;
mod read;
mod write;

use auth::{Auth, Caller};

/// How long shutdown waits for requests under way to finish before closing their connections.
/// The API's requests are quick: a reload, or a cancel waiting for its commit, takes well under
/// this.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// What every handler can reach.
#[derive(Debug, Clone)]
pub(crate) struct State {
    /// Gone once the service is dropped; see the module documentation.
    service: Weak<Service>,
    auth: Arc<Auth>,
    audit: Arc<Audit>,
    /// Set once the server is stopping: its graceful shutdown waits for requests, but not for the
    /// websockets they upgraded to, which watch this to close.
    stopping: watch::Sender<bool>,
    live: Arc<live::Sockets>,
    /// The message log's searches under way: see [`messages::MAX_SEARCHES`].
    searches: Arc<Semaphore>,
}

impl State {
    /// The service, or 503 once it's gone: the gateway is shutting down.
    fn service(&self) -> Result<Arc<Service>, ApiError> {
        self.service.upgrade().ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the gateway is stopping"))
    }

    /// Records `entry` as refused with `error` and answers `error`; or 503 if the line can't be
    /// written. An identified caller's refusal (role known) is always written, anyone else's
    /// within the audit log's budget for refusals; see [`crate::audit`].
    async fn refused(&self, mut entry: Entry, error: ApiError) -> ApiError {
        entry.result = format!("refused: {}", error.message);
        let recorded = if entry.role.is_some() {
            self.audit.record(&entry).await
        } else {
            self.audit.record_refusal(&entry).await
        };
        match recorded {
            Ok(()) => error,
            Err(e) => audit_failed(&e),
        }
    }
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
        .route("/api/audit", get(read_audit))
        .route("/api/sessions", get(read::sessions))
        .route("/api/orders", get(read::orders))
        .route("/api/orders/{order_id}", get(read::order))
        .route("/api/orders/{order_id}/messages", get(messages::order_messages))
        .route("/api/messages", get(messages::messages))
        .route("/api/risk", get(read::risk))
        .route("/api/risk/limits", get(read::risk_limits))
        .route("/api/routes", get(read::routes))
        .route("/api/routes/test", get(read::route_test))
        .route("/api/live", get(live::live))
        .route("/api/sessions/{id}/pause", post(write::pause))
        .route("/api/sessions/{id}/resume", post(write::resume))
        .route("/api/sessions/{id}/resend", post(write::resend))
        .route("/api/sessions/{id}/sequence", post(write::sequence))
        .route("/api/sessions/{id}/reset", post(write::reset))
        .route("/api/orders/{order_id}/cancel", post(write::cancel_order))
        .route("/api/orders/{order_id}/status", post(write::request_status))
        .route("/api/risk/block", post(write::block))
        .route("/api/risk/unblock", post(write::unblock))
        .route("/api/risk/cancel-all", post(write::cancel_all))
        .route("/api/risk/limits/defaults", patch(write::set_default_limits))
        .route("/api/risk/limits/client/{comp_id}", patch(write::set_client_limits).delete(write::delete_client_limits))
        .route("/api/reload", post(write::reload))
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

/// A reason's length at most, in bytes: a sentence or a ticket number, not a document.
const MAX_REASON_LEN: usize = 1_000;

/// Entries `GET /api/audit` returns without a `limit`.
const AUDIT_READ_DEFAULT: usize = 100;

/// A mutation as the audit log records it: what [`audited`] needs besides the action itself.
#[derive(Debug)]
pub(crate) struct Mutation {
    /// Its name, as the audit log has it: `pause`, `cancel_order`, `block`...
    pub(crate) action: &'static str,
    pub(crate) target: Option<String>,
    /// Its other arguments. Never secrets.
    pub(crate) params: serde_json::Value,
    /// The body's `reason`, which every mutation needs, non-empty.
    pub(crate) reason: Option<String>,
}

/// What an action answers: its answer, and what happened if not simply done, for the audit log's
/// second line (`unchanged: already blocked`).
pub(crate) type Outcome<T> = Result<(T, Option<String>), ApiError>;

/// Runs `action`, a mutation by `caller`, once its audit line is written; see [`crate::audit`].
///
/// Refuses it, recording the refusal, if `allowed` is an error (the caller's role or scope, or a
/// target that doesn't exist) or the reason is missing, blank or too long (400). Then runs it as
/// [`run_audited`] does. A handler answers 503 before it, unrecorded, once the gateway is
/// stopping.
pub(crate) async fn audited<T: Send + 'static>(
    state: &State,
    caller: &Caller,
    mutation: Mutation,
    allowed: Result<(), ApiError>,
    action: impl Future<Output = Outcome<T>> + Send + 'static,
) -> Result<T, ApiError> {
    let reason = mutation.reason.unwrap_or_default();
    let too_long = reason.len() > MAX_REASON_LEN;
    let refusal = allowed.err().or_else(|| {
        if reason.trim().is_empty() {
            Some(ApiError::new(StatusCode::BAD_REQUEST, "a mutation needs a reason"))
        } else if too_long {
            Some(ApiError::new(StatusCode::BAD_REQUEST, format!("the reason is over {MAX_REASON_LEN} bytes")))
        } else {
            None
        }
    });
    let mut entry = entry_for(caller, mutation.action, mutation.target);
    entry.params = mutation.params;
    // A reason too long isn't kept: the refusal says why.
    entry.reason = (!reason.is_empty() && !too_long).then_some(reason);
    if let Some(error) = refusal {
        return Err(state.refused(entry, error).await);
    }
    run_audited(state.audit.clone(), entry, action).await
}

/// Records `entry` as allowed, runs `action`, and records a second line if it wasn't simply done:
/// `failed: ...` if it answered an error or panicked, or the outcome it reported. Answers 503,
/// not running it, if the first line can't be written.
///
/// All of it runs on a task of its own, so a client that gives up midway can't leave an allowed
/// line without its action, nor an action without its outcome.
pub(crate) async fn run_audited<T: Send + 'static>(
    audit: Arc<Audit>,
    mut entry: Entry,
    action: impl Future<Output = Outcome<T>> + Send + 'static,
) -> Result<T, ApiError> {
    let run = async move {
        entry.result = "allowed".to_string();
        audit.record(&entry).await.map_err(|e| audit_failed(&e))?;
        // A task of its own too, so a panic in it still gets its outcome line.
        let outcome = tokio::spawn(action).await.unwrap_or_else(|e| {
            error!(action = entry.action, "an API action panicked: {e}");
            Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "panicked"))
        });
        let second = match &outcome {
            Ok((_, note)) => note.clone(),
            Err(failed) => Some(format!("failed: {}", failed.message)),
        };
        if let Some(result) = second {
            entry.time = Utc::now();
            entry.result = result;
            if let Err(e) = audit.record(&entry).await {
                // Done is done: the caller hears what happened, whatever the log.
                error!(action = entry.action, "cannot write an action's outcome to the audit log: {e}");
            }
        }
        outcome.map(|(answer, _)| answer)
    };
    tokio::spawn(run).await.unwrap_or_else(|e| {
        error!("an audited action's task failed: {e}");
        Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "the action failed"))
    })
}

/// An audit line for `caller` doing `action` to `target`, with no params, reason or result yet.
pub(crate) fn entry_for(caller: &Caller, action: &str, target: Option<String>) -> Entry {
    Entry::new(Some(caller.user.clone()), Some(caller.role), caller.ip, action, target)
}

/// The answer, 503, when the audit log can't be written: whatever it was for didn't happen.
fn audit_failed(e: &io::Error) -> ApiError {
    error!("cannot write the audit log: {e}");
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the audit log can't be written: nothing was done")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    /// RFC 3339.
    since: Option<DateTime<Utc>>,
    limit: Option<usize>,
}

/// `GET /api/audit?since=&limit=` [admin]: the audit log's last `limit` entries (at most
/// [`MAX_READ`](crate::audit::MAX_READ), by default [`AUDIT_READ_DEFAULT`]) from `since`, oldest
/// first.
async fn read_audit(
    axum::extract::State(state): axum::extract::State<State>,
    caller: Caller,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Vec<Entry>>, ApiError> {
    if let Err(error) = caller.require(Role::Admin) {
        return Err(state.refused(entry_for(&caller, "read_audit", None), error).await);
    }
    let Query(query) = query.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let entries = state.audit.read(query.since, query.limit.unwrap_or(AUDIT_READ_DEFAULT)).await.map_err(|e| {
        error!("cannot read the audit log: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot read the audit log: {e}"))
    })?;
    Ok(Json(entries))
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
    audit: Arc<Audit>,
}

impl Bound {
    /// Binds `config.listen`, or takes `given`, loads `config.tls`'s certificate and key and
    /// `config.users`, and opens `config.audit`, creating it if missing. The audit log stays the
    /// one opened here until a restart.
    pub(crate) async fn bind(config: &Api, given: Option<std::net::TcpListener>) -> Result<Self, Error> {
        let auth = Arc::new(Auth::load(config).map_err(Error::new)?);
        let audit = Audit::open(&config.audit)
            .map_err(|e| Error::new(format!("cannot open the audit log {}: {e}", config.audit.display())))?;
        let audit = Arc::new(audit);
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
            audit,
        })
    }

    /// Serves the API's routes, with handlers reaching `service`.
    pub(crate) fn serve(self, service: Weak<Service>) -> Server {
        let local_addr = self.listener.local_addr().unwrap_or(self.config.listen);
        let stopping = watch::Sender::new(false);
        let app = router(State {
            service,
            auth: self.auth.clone(),
            audit: self.audit,
            stopping: stopping.clone(),
            live: Arc::default(),
            searches: Arc::new(Semaphore::new(messages::MAX_SEARCHES)),
        });
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
            stopping,
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
    /// See [`State::stopping`].
    stopping: watch::Sender<bool>,
    task: JoinHandle<io::Result<()>>,
}

impl Server {
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting, closes the websockets, and waits up to [`SHUTDOWN_WAIT`] for the requests
    /// under way.
    pub(crate) async fn shutdown(self) {
        self.stopping.send_replace(true);
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
        self.stopping.send_replace(true);
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

    /// A state with `auth`, no service, and an audit log in a new directory, which lives as long
    /// as the directory returned.
    pub(crate) fn state(auth: Arc<Auth>) -> (State, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let audit = Arc::new(Audit::open(&dir.path().join("audit.jsonl")).unwrap());
        let stopping = watch::Sender::new(false);
        let searches = Arc::new(Semaphore::new(messages::MAX_SEARCHES));
        (State { service: Weak::new(), auth, audit, stopping, live: Arc::default(), searches }, dir)
    }

    /// The API's routes, and `/test/peer` answering the client's address as handlers see it.
    fn app() -> (axum::Router, tempfile::TempDir) {
        let auth = Auth::new(std::path::PathBuf::new(), crate::auth::Users::default(), false, false);
        let (state, dir) = state(Arc::new(auth));
        let app =
            router(state).route("/test/peer", get(async |ConnectInfo(addr): ConnectInfo<SocketAddr>| addr.to_string()));
        (app, dir)
    }

    /// Serves [`app`] on a plain loopback listener, returning its address, its stop and its
    /// audit log's directory.
    async fn serve_plain() -> (SocketAddr, oneshot::Sender<()>, tempfile::TempDir) {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let (app, dir) = app();
        spawn(listener::Plain::new(tcp), app, stopped);
        (addr, stop, dir)
    }

    pub(crate) struct Answer {
        pub(crate) status: StatusCode,
        pub(crate) headers: axum::http::HeaderMap,
        pub(crate) body: serde_json::Value,
    }

    /// `method path` with `headers` and a JSON `body`, to `app` in-process.
    pub(crate) async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Answer {
        use tower::ServiceExt;
        let mut request =
            axum::http::Request::builder().method(method).uri(path).header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(axum::body::Body::from(body.to_string())).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body = if body.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&body).unwrap() };
        Answer { status: parts.status, headers: parts.headers, body }
    }

    mod audited {
        use super::*;
        use crate::audit::tests::make_read_only;
        use crate::auth::Users;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Deserialize)]
        struct Flip {
            reason: Option<String>,
            /// `fail` answers 409 without flipping; `panic` panics.
            how: Option<String>,
        }

        /// A test mutation, `POST /test/flip {reason, how}` [operator]: flips `switch` on,
        /// reporting `unchanged: already on` if it was.
        struct Fixture {
            app: axum::Router,
            switch: Arc<AtomicBool>,
            audit: Arc<Audit>,
            dir: tempfile::TempDir,
            /// Bearer headers' values: a viewer's, an operator's and an admin's.
            viewer: String,
            operator: String,
            admin: String,
        }

        fn fixture() -> Fixture {
            let mut users = Users::default();
            let mut bearer = |name: &str, role: Role| {
                users.add(name, role, Vec::new(), "a long password").unwrap();
                format!("Bearer {}", users.add_token(name).unwrap())
            };
            let (viewer, operator, admin) =
                (bearer("vic", Role::Viewer), bearer("olga", Role::Operator), bearer("ada", Role::Admin));
            let (state, dir) = state(Arc::new(Auth::new(std::path::PathBuf::new(), users, false, false)));
            let switch = Arc::new(AtomicBool::new(false));
            let flipped = switch.clone();
            let flip = move |axum::extract::State(state): axum::extract::State<State>,
                             caller: Caller,
                             Json(flip): Json<Flip>| {
                let flipped = flipped.clone();
                async move {
                    let mutation = Mutation {
                        action: "flip",
                        target: Some("switch".into()),
                        params: serde_json::json!({ "to": true }),
                        reason: flip.reason,
                    };
                    let allowed = caller.require(Role::Operator);
                    audited(&state, &caller, mutation, allowed, async move {
                        match flip.how.as_deref() {
                            Some("fail") => Err(ApiError::new(StatusCode::CONFLICT, "told to fail")),
                            Some("panic") => panic!("told to panic"),
                            _ if flipped.swap(true, Ordering::SeqCst) => {
                                Ok((StatusCode::NO_CONTENT, Some("unchanged: already on".to_string())))
                            }
                            _ => Ok((StatusCode::NO_CONTENT, None)),
                        }
                    })
                    .await
                }
            };
            let test = axum::Router::new()
                .route("/test/flip", post(flip))
                .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth::authenticate))
                .with_state(state.clone());
            let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
            let audit = state.audit.clone();
            let app = router(state).merge(test).layer(axum::extract::connect_info::MockConnectInfo(peer));
            Fixture { app, switch, audit, dir, viewer, operator, admin }
        }

        impl Fixture {
            async fn flip(&self, bearer: &str, body: &str) -> StatusCode {
                send(&self.app, "POST", "/test/flip", &[("authorization", bearer)], body).await.status
            }

            fn path(&self) -> std::path::PathBuf {
                self.dir.path().join("audit.jsonl")
            }

            fn lines(&self) -> Vec<serde_json::Value> {
                let text = std::fs::read_to_string(self.path()).unwrap_or_default();
                text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
            }
        }

        /// An allowed mutation is recorded before it runs, and done is one line; one that's
        /// refused (role, no reason, a blank one, one too long) is recorded as refused and doesn't
        /// run; one allowed but not done gets a second line saying why.
        #[tokio::test]
        async fn mutations_are_recorded_allowed_refused_and_failed() {
            let fixture = fixture();
            let reason = r#"{"reason":"testing"}"#;
            assert_eq!(fixture.flip(&fixture.viewer, reason).await, StatusCode::FORBIDDEN);
            for body in ["{}", r#"{"reason":"  "}"#, &format!(r#"{{"reason":"{}"}}"#, "x".repeat(MAX_REASON_LEN + 1))] {
                assert_eq!(fixture.flip(&fixture.operator, body).await, StatusCode::BAD_REQUEST, "{body}");
            }
            assert!(!fixture.switch.load(Ordering::SeqCst));
            assert_eq!(fixture.flip(&fixture.operator, reason).await, StatusCode::NO_CONTENT);
            assert!(fixture.switch.load(Ordering::SeqCst));
            let how = |how: &str| format!(r#"{{"reason":"testing","how":"{how}"}}"#);
            assert_eq!(fixture.flip(&fixture.admin, &how("fail")).await, StatusCode::CONFLICT);
            assert_eq!(fixture.flip(&fixture.admin, reason).await, StatusCode::NO_CONTENT);
            assert_eq!(fixture.flip(&fixture.admin, &how("panic")).await, StatusCode::INTERNAL_SERVER_ERROR);

            let lines = fixture.lines();
            let results: Vec<_> = lines.iter().map(|line| line["result"].as_str().unwrap()).collect();
            assert_eq!(
                results,
                [
                    "refused: needs the operator role; vic is viewer",
                    "refused: a mutation needs a reason",
                    "refused: a mutation needs a reason",
                    &format!("refused: the reason is over {MAX_REASON_LEN} bytes"),
                    "allowed",
                    "allowed",
                    "failed: told to fail",
                    "allowed",
                    "unchanged: already on",
                    "allowed",
                    "failed: panicked",
                ]
            );
            let allowed = &lines[4];
            assert_eq!(allowed["user"], "olga");
            assert_eq!(allowed["role"], "operator");
            assert_eq!(allowed["ip"], "192.0.2.1");
            assert_eq!(allowed["action"], "flip");
            assert_eq!(allowed["target"], "switch");
            assert_eq!(allowed["params"], serde_json::json!({ "to": true }));
            assert_eq!(allowed["reason"], "testing");
            assert!(allowed["time"].as_str().unwrap().ends_with('Z'));
            assert_eq!(lines[2]["reason"], "  ");
            assert_eq!(lines[3]["reason"], serde_json::Value::Null);
            assert_eq!(lines[6]["user"], "ada");
        }

        /// An identified caller's refusal is written even with the refusals' budget spent; an
        /// unidentified one's then isn't, and still answers its own status.
        #[tokio::test]
        async fn only_unidentified_refusals_are_budgeted() {
            let fixture = fixture();
            while fixture.audit.spend_refusal(std::time::Instant::now()) {}
            let reason = r#"{"reason":"testing"}"#;
            assert_eq!(fixture.flip(&fixture.viewer, reason).await, StatusCode::FORBIDDEN);
            assert_eq!(fixture.flip("Bearer jb_wrong", reason).await, StatusCode::UNAUTHORIZED);
            let lines = fixture.lines();
            let [line] = lines.as_slice() else { panic!("{lines:?}") };
            assert_eq!(line["user"], "vic");
        }

        /// A request given up on once its action has started doesn't stop it: the action
        /// finishes, and its outcome line is written.
        #[tokio::test]
        async fn an_action_outlives_its_request() {
            let fixture = fixture();
            let started = Arc::new(tokio::sync::Notify::new());
            let go = Arc::new(tokio::sync::Notify::new());
            let (started_2, go_2) = (started.clone(), go.clone());
            let action = async move {
                started_2.notify_one();
                go_2.notified().await;
                Ok(((), Some("unchanged: test".to_string())))
            };
            let entry = Entry::new(Some("olga".into()), Some(Role::Operator), [192, 0, 2, 1].into(), "test", None);
            let request = tokio::spawn(run_audited(fixture.audit.clone(), entry, action));
            started.notified().await;
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            go.notify_one();
            for _ in 0..500 {
                if fixture.lines().len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let results: Vec<_> = fixture.lines().iter().map(|line| line["result"].clone()).collect();
            assert_eq!(results, ["allowed", "unchanged: test"]);
        }

        /// A mutation whose line can't be written, the file read-only or its directory gone,
        /// answers 503 and doesn't run.
        #[tokio::test]
        async fn a_mutation_whose_line_fails_doesnt_run() {
            let fixture = fixture();
            make_read_only(&fixture.path());
            let reason = r#"{"reason":"testing"}"#;
            assert_eq!(fixture.flip(&fixture.operator, reason).await, StatusCode::SERVICE_UNAVAILABLE);
            assert!(!fixture.switch.load(Ordering::SeqCst));
            assert!(fixture.lines().is_empty());

            std::fs::remove_dir_all(fixture.dir.path()).unwrap();
            assert_eq!(fixture.flip(&fixture.operator, reason).await, StatusCode::SERVICE_UNAVAILABLE);
            assert!(!fixture.switch.load(Ordering::SeqCst));
        }

        /// An admin reads the log back, from `since`, the last `limit`; an operator is refused,
        /// and that's recorded.
        #[tokio::test]
        async fn the_log_reads_back() {
            let fixture = fixture();
            let audit = Audit::open(&fixture.path()).unwrap();
            for hour in ["09", "10", "11"] {
                let entry = Entry {
                    time: format!("2026-10-11T{hour}:00:00Z").parse().unwrap(),
                    user: Some("olga".into()),
                    role: Some(Role::Operator),
                    ip: [192, 0, 2, 1].into(),
                    action: "flip".into(),
                    target: Some(format!("at {hour}")),
                    params: serde_json::Value::Null,
                    reason: Some("testing".into()),
                    result: "allowed".into(),
                };
                audit.record(&entry).await.unwrap();
            }
            let read = async |query: &str| {
                send(&fixture.app, "GET", &format!("/api/audit{query}"), &[("authorization", &fixture.admin)], "").await
            };
            let targets = |answer: Answer| {
                assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
                let entries = answer.body.as_array().unwrap().clone();
                entries.iter().map(|e| e["target"].as_str().unwrap().to_string()).collect::<Vec<_>>()
            };
            assert_eq!(targets(read("").await), ["at 09", "at 10", "at 11"]);
            assert_eq!(targets(read("?since=2026-10-11T10:00:00Z").await), ["at 10", "at 11"]);
            assert_eq!(targets(read("?since=2026-10-11T09:30:00Z&limit=1").await), ["at 11"]);
            assert_eq!(read("?since=yesterday").await.status, StatusCode::BAD_REQUEST);

            let answer = send(&fixture.app, "GET", "/api/audit", &[("authorization", &fixture.operator)], "").await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN);
            let lines = fixture.lines();
            let last = lines.last().unwrap();
            assert_eq!(last["action"], "read_audit");
            assert_eq!(last["result"], "refused: needs the admin role; olga is operator");
        }
    }

    /// Unknown paths and methods answer the API's JSON error, not axum's plain text.
    #[tokio::test]
    async fn unknown_paths_and_methods_are_json_errors() {
        let (addr, _stop, _dir) = serve_plain().await;
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
        let (addr, _stop, _dir) = serve_plain().await;
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
        async fn serve_tls() -> (SocketAddr, String, oneshot::Sender<()>, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let (cert, key) = localhost_cert();
            let files = ApiTls { cert: dir.path().join("api.pem"), key: dir.path().join("api.key") };
            std::fs::write(&files.cert, &cert).unwrap();
            std::fs::write(&files.key, key).unwrap();
            let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = tcp.local_addr().unwrap();
            let (stop, stopped) = oneshot::channel();
            let (app, audit_dir) = app();
            spawn(listener::Tls::new(tcp, listener::server(&files).unwrap().acceptor()), app, stopped);
            (addr, cert, stop, audit_dir)
        }

        #[tokio::test]
        async fn handlers_see_the_clients_address_over_tls() {
            let (addr, cert, _stop, _dir) = serve_tls().await;
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let client = tcp.local_addr().unwrap();
            let response = exchange(tls_client(&cert).await.unwrap().connect(tcp).await.unwrap(), "GET", "/test/peer");
            assert!(response.await.ends_with(&client.to_string()));
        }

        /// A client that connects and sends nothing doesn't hold up others' handshakes.
        #[tokio::test]
        async fn a_stalled_connection_doesnt_hold_up_tls_handshakes() {
            let (addr, cert, _stop, _dir) = serve_tls().await;
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
