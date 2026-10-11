//! Who is calling: logins and their sessions, tokens, the CSRF check, and the login limit.
//!
//! Every route but health and login sits behind [`authenticate`], which puts a [`Caller`] in the
//! request or refuses it, so a handler can't forget to check. A caller is either:
//!
//! - **a token**, `Authorization: Bearer jb_...`, for scripts. A header that's there but wrong is
//!   refused, never passed over for the cookie.
//! - **a session**, from `POST /api/login`: the `jetbridge_session` cookie, which a browser sends
//!   by itself, so every request but a GET or HEAD must also carry `X-CSRF-Token`, the session's
//!   own token, which a page on another site can't read. Sessions are kept in memory: a restart
//!   logs everyone out.
//!
//! Each request reads the user afresh from the users file in use, so a reload that removes a user
//! or a token, or changes a role or clients, applies from the next request; a session whose user
//! was removed, or whose password changed, ends.
//!
//! Auth outcomes all pass through [`login`], [`logout`] and [`refuse`]; they're logged there.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, FromRequestParts, Request};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64ct::{Base64UrlUnpadded, Encoding};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use super::ApiError;
use crate::auth::{MAX_NAME_LEN, Role, User, Users, random, verify_password};
use crate::book::{ClientId, Scope};
use crate::config::Api;

/// The session cookie's name.
pub(crate) const SESSION_COOKIE: &str = "jetbridge_session";

/// The header a session's every request but a GET or HEAD carries its CSRF token in.
const CSRF_HEADER: &str = "x-csrf-token";

/// How long a session lasts unused: an operator's working day, so a console left open over a
/// shift stays logged in, and one left overnight doesn't.
pub(crate) const SESSION_IDLE: Duration = Duration::from_secs(8 * 60 * 60);

/// Sessions at once. A handful of operators need a few each; when full, the least recently used
/// goes. Refusing new logins instead would let a script that logs in without logging out lock
/// everyone out of the kill switch until its sessions idle out; evicting costs a forgotten
/// console its session, and the one in use is never the least recently used.
const MAX_SESSIONS: usize = 1024;

/// Failed logins a user name or an IP address may have in a [`LOGIN_WINDOW`]; then logins for it
/// are refused, 429, until the window passes.
const LOGIN_FAILURES: u32 = 5;

/// See [`LOGIN_FAILURES`]. The window starts at its first failure.
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// User names and IP addresses the login limit keeps a window for. Expired windows are dropped
/// when it fills; if it's still full, logins for names and addresses not in it are refused until
/// some expire: past this many in a minute, someone is guessing, and refusing beats forgetting
/// who.
const MAX_LOGIN_WINDOWS: usize = 10_000;

/// Password checks at once. Each takes argon2's 19 MiB and some tens of milliseconds of a
/// blocking thread; more wait their turn.
const MAX_PASSWORD_CHECKS: usize = 4;

/// Where the time comes from: a test's, to pass windows and timeouts without waiting.
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The users, their sessions and the login limit.
pub(crate) struct Auth {
    /// The file the users come from, reread on reload.
    path: PathBuf,
    users: RwLock<Arc<Users>>,
    /// By session ID.
    sessions: Mutex<HashMap<String, Session>>,
    failures: Mutex<HashMap<LimitKey, Window>>,
    checks: Semaphore,
    /// Whether the session cookie is `Secure`: served over TLS, here or at a proxy.
    secure: bool,
    /// Whether the client's address is the last `X-Forwarded-For` hop rather than the peer's.
    behind_proxy: bool,
    clock: Clock,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth").field("path", &self.path).finish_non_exhaustive()
    }
}

struct Session {
    user: String,
    /// The user's password hash at login: a changed password ends the session.
    password: String,
    csrf: String,
    last_seen: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LimitKey {
    User(String),
    Ip(IpAddr),
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    failures: u32,
}

/// Why a request wasn't authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    NoCredentials,
    BadPassword,
    BadToken,
    /// No such session: never one, logged out, idle too long, its user removed or their password
    /// changed, or the gateway restarted.
    SessionEnded,
    Csrf,
    Limited,
}

impl Refusal {
    fn status(self) -> StatusCode {
        match self {
            Self::NoCredentials | Self::BadPassword | Self::BadToken | Self::SessionEnded => StatusCode::UNAUTHORIZED,
            Self::Csrf => StatusCode::FORBIDDEN,
            Self::Limited => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::NoCredentials => "log in, or send Authorization: Bearer <token>",
            Self::BadPassword => "wrong user or password",
            Self::BadToken => "unknown or revoked token",
            Self::SessionEnded => "the session has ended: log in again",
            Self::Csrf => "missing or wrong X-CSRF-Token",
            Self::Limited => "too many failed logins: try again in a minute",
        }
    }
}

impl From<Refusal> for ApiError {
    fn from(refusal: Refusal) -> Self {
        Self::new(refusal.status(), refusal.message())
    }
}

/// Logs a refusal and answers it.
fn refuse(refusal: Refusal, user: Option<&str>, ip: IpAddr) -> ApiError {
    info!(user, %ip, why = refusal.message(), "API caller refused");
    refusal.into()
}

impl Auth {
    /// The users in `config.users`, the cookie `Secure` with TLS here or at a proxy.
    pub(crate) fn load(config: &Api) -> Result<Self, String> {
        let users = Users::load(&config.users)?;
        let secure = config.tls.is_some() || config.behind_proxy;
        Ok(Self::new(config.users.clone(), users, secure, config.behind_proxy))
    }

    pub(super) fn new(path: PathBuf, users: Users, secure: bool, behind_proxy: bool) -> Self {
        Self {
            path,
            users: RwLock::new(Arc::new(users)),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            checks: Semaphore::new(MAX_PASSWORD_CHECKS),
            secure,
            behind_proxy,
            clock: Arc::new(Instant::now),
        }
    }

    /// Rereads the users file, returning a warning if it doesn't load: the users in use are kept.
    pub(crate) fn reload(&self) -> Option<String> {
        match Users::load(&self.path) {
            Ok(users) => {
                info!(users = users.names().count(), "reloaded the users file");
                *self.users.write().expect("users lock poisoned") = Arc::new(users);
                None
            }
            Err(e) => Some(format!("keeping the users in use: {e}")),
        }
    }

    fn users(&self) -> Arc<Users> {
        self.users.read().expect("users lock poisoned").clone()
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    /// The client's address: the peer's, or behind a proxy the last `X-Forwarded-For` hop, the
    /// one the proxy added (those before it are the client's to write). A proxy that sends none,
    /// or garbage, is the client.
    pub(crate) fn client_ip(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let forwarded = || {
            let last = headers.get_all("x-forwarded-for").iter().next_back()?.to_str().ok()?;
            last.rsplit(',').next()?.trim().parse::<IpAddr>().ok()
        };
        let ip = if self.behind_proxy { forwarded() } else { None };
        ip.unwrap_or(peer.ip()).to_canonical()
    }

    /// Checks `password` for user `name`, from `ip`, and starts a session. An unknown user costs
    /// a password check too, so how long a refusal takes doesn't say whether the user exists.
    async fn login(&self, name: &str, password: String, ip: IpAddr) -> Result<LoggedIn, Refusal> {
        if name.len() > MAX_NAME_LEN {
            // No user has such a name: nothing to guess, and nothing worth a window.
            return Err(Refusal::BadPassword);
        }
        self.check_limit(name, ip)?;
        let users = self.users();
        let user = users.get(name);
        let hash = user.map(|user| user.password.clone());
        let correct = {
            let _turn = self.checks.acquire().await.expect("the semaphore is never closed");
            // Again, as failures counted while this waited its turn may have reached the limit:
            // else a burst of guesses all passing the first check would each get a try.
            self.check_limit(name, ip)?;
            tokio::task::spawn_blocking(move || match hash {
                Some(hash) => verify_password(&hash, &password),
                None => {
                    std::hint::black_box(verify_password(dummy_hash(), &password));
                    false
                }
            })
            .await
            .unwrap_or(false)
        };
        let Some(user) = user.filter(|_| correct) else {
            self.count_failure(name, ip);
            return Err(Refusal::BadPassword);
        };
        let session = Base64UrlUnpadded::encode_string(&random::<32>());
        let csrf = Base64UrlUnpadded::encode_string(&random::<32>());
        self.add_session(
            session.clone(),
            Session {
                user: name.to_string(),
                password: user.password.clone(),
                csrf: csrf.clone(),
                last_seen: self.now(),
            },
        );
        Ok(LoggedIn { session, me: Me::new(name, user, Some(csrf)) })
    }

    /// Refuses a login for `name` or `ip` if either has had too many failures lately, or if the
    /// limit is full of others.
    fn check_limit(&self, name: &str, ip: IpAddr) -> Result<(), Refusal> {
        let now = self.now();
        let mut failures = self.failures.lock().expect("login limit lock poisoned");
        let keys = [LimitKey::User(name.to_string()), LimitKey::Ip(ip)];
        if failures.len() >= MAX_LOGIN_WINDOWS {
            failures.retain(|_, window| !window.expired(now));
            if failures.len() >= MAX_LOGIN_WINDOWS && keys.iter().all(|key| !failures.contains_key(key)) {
                warn!(%ip, "the login limit is full: refusing logins from new names and addresses");
                return Err(Refusal::Limited);
            }
        }
        let limited = |key| failures.get(key).is_some_and(|w: &Window| !w.expired(now) && w.failures >= LOGIN_FAILURES);
        if keys.iter().any(limited) { Err(Refusal::Limited) } else { Ok(()) }
    }

    fn count_failure(&self, name: &str, ip: IpAddr) {
        let now = self.now();
        let mut failures = self.failures.lock().expect("login limit lock poisoned");
        for key in [LimitKey::User(name.to_string()), LimitKey::Ip(ip)] {
            let full = failures.len() >= MAX_LOGIN_WINDOWS;
            match failures.get_mut(&key) {
                Some(window) if !window.expired(now) => window.failures += 1,
                Some(window) => *window = Window { start: now, failures: 1 },
                // The next login is refused by check_limit anyway.
                None if full => {}
                None => {
                    failures.insert(key, Window { start: now, failures: 1 });
                }
            }
        }
    }

    fn add_session(&self, id: String, session: Session) {
        let now = self.now();
        let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
        if sessions.len() >= MAX_SESSIONS {
            sessions.retain(|_, session| now.saturating_duration_since(session.last_seen) < SESSION_IDLE);
        }
        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions.iter().min_by_key(|(_, s)| s.last_seen).map(|(id, _)| id.clone());
            let oldest = oldest.expect("full, so not empty");
            info!(user = sessions[&oldest].user, "too many API sessions: ending the least recently used");
            sessions.remove(&oldest);
        }
        assert!(sessions.len() < MAX_SESSIONS);
        sessions.insert(id, session);
    }

    /// The caller of a request with `headers`, by `method`, from `ip`; see the module
    /// documentation.
    fn authenticate(&self, method: &Method, headers: &HeaderMap, ip: IpAddr) -> Result<Caller, Refusal> {
        let users = self.users();
        if let Some(value) = headers.get(AUTHORIZATION) {
            let token = value.to_str().ok().and_then(|v| v.strip_prefix("Bearer ")).ok_or(Refusal::BadToken)?;
            let (name, user) = users.by_token(token.trim()).ok_or(Refusal::BadToken)?;
            return Ok(Caller::new(name, user, ip, None));
        }
        let id = session_cookie(headers).ok_or(Refusal::NoCredentials)?;
        let now = self.now();
        let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
        let session = sessions.get_mut(id).ok_or(Refusal::SessionEnded)?;
        let user = users.get(&session.user).filter(|user| user.password == session.password);
        let (Some(user), false) = (user, now.saturating_duration_since(session.last_seen) >= SESSION_IDLE) else {
            sessions.remove(id);
            return Err(Refusal::SessionEnded);
        };
        if !matches!(*method, Method::GET | Method::HEAD) {
            let sent = headers.get(CSRF_HEADER).map_or(&[][..], |value| value.as_bytes());
            if !bool::from(sent.ct_eq(session.csrf.as_bytes())) {
                return Err(Refusal::Csrf);
            }
        }
        session.last_seen = now;
        let reference = SessionRef { id: id.to_string(), csrf: session.csrf.clone() };
        Ok(Caller::new(&session.user, user, ip, Some(reference)))
    }

    fn end_session(&self, id: &str) {
        self.sessions.lock().expect("sessions lock poisoned").remove(id);
    }

    /// The `Set-Cookie` value for session `id`; an empty `id` clears the cookie.
    fn cookie(&self, id: &str) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        let expire = if id.is_empty() { "; Max-Age=0" } else { "" };
        format!("{SESSION_COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/api{secure}{expire}")
    }
}

impl Window {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= LOGIN_WINDOW
    }
}

/// A hash no password matches in practice, checked for unknown users so they cost what a wrong
/// password does. Made once, with the cost [`hash_password`](crate::auth::hash_password) gives
/// users.
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| crate::auth::hash_password(&Base64UrlUnpadded::encode_string(&random::<32>())))
}

/// The session cookie's value, if the request has one.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|cookie| cookie.trim().strip_prefix(SESSION_COOKIE)?.strip_prefix('='))
}

/// Who is calling, for handlers: take it as an argument. Only routes behind [`authenticate`]
/// have one; anywhere else, taking it answers 500, never an unauthenticated call.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    pub(crate) user: String,
    pub(crate) role: Role,
    /// The CompIDs the caller may act on; empty, all.
    pub(crate) clients: Vec<String>,
    pub(crate) ip: IpAddr,
    /// With the cookie, not a token.
    session: Option<SessionRef>,
}

#[derive(Clone)]
struct SessionRef {
    id: String,
    csrf: String,
}

/// Without the secrets.
impl fmt::Debug for SessionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionRef")
    }
}

impl Caller {
    fn new(name: &str, user: &User, ip: IpAddr, session: Option<SessionRef>) -> Self {
        Self { user: name.to_string(), role: user.role, clients: user.clients.clone(), ip, session }
    }

    /// Refuses, 403, a caller whose role is below `role`.
    #[cfg_attr(not(test), expect(dead_code, reason = "the endpoints after login check roles"))]
    pub(crate) fn require(&self, role: Role) -> Result<(), ApiError> {
        if self.role >= role {
            Ok(())
        } else {
            Err(ApiError::new(StatusCode::FORBIDDEN, format!("needs the {role} role; {} is {}", self.user, self.role)))
        }
    }

    /// Whether the caller may act on `target`: an unscoped caller on anything, a scoped one on
    /// its own CompIDs' clients only (so never on [`Scope::All`]).
    #[cfg_attr(not(test), expect(dead_code, reason = "the mutations check scopes"))]
    pub(crate) fn may_act_on<T: Target + ?Sized>(&self, target: &T) -> bool {
        self.clients.is_empty() || target.comp_id().is_some_and(|comp_id| self.clients.iter().any(|c| c == comp_id))
    }
}

/// What a scoped caller may or may not act on: whose CompID it is, if it's one CompID's.
pub(crate) trait Target {
    fn comp_id(&self) -> Option<&str>;
}

impl Target for str {
    fn comp_id(&self) -> Option<&str> {
        Some(self)
    }
}

impl Target for ClientId {
    fn comp_id(&self) -> Option<&str> {
        Some(ClientId::comp_id(self))
    }
}

impl Target for Scope {
    fn comp_id(&self) -> Option<&str> {
        match self {
            Self::Session(client) => Some(client.comp_id()),
            Self::CompId(comp_id) => Some(comp_id),
            Self::All => None,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        parts.extensions.get::<Self>().cloned().ok_or_else(|| {
            debug_assert!(false, "{} takes a Caller outside authenticate", parts.uri.path());
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "this route isn't behind authentication")
        })
    }
}

/// The middleware in front of every route but health and login: the request goes on with its
/// [`Caller`], or is refused.
pub(super) async fn authenticate(
    axum::extract::State(state): axum::extract::State<super::State>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let ip = state.auth.client_ip(peer, request.headers());
    match state.auth.authenticate(request.method(), request.headers(), ip) {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(refusal) => refuse(refusal, None, ip).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Login {
    user: String,
    password: String,
}

/// A caller as the API shows them: `POST /api/login`'s and `GET /api/me`'s answer.
#[derive(Debug, Serialize)]
pub(super) struct Me {
    user: String,
    role: Role,
    clients: Vec<String>,
    /// With a session: the token its requests but GETs carry in `X-CSRF-Token`. A page reloaded
    /// gets it again from `GET /api/me`, which another site's page can't read.
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf: Option<String>,
}

impl Me {
    fn new(name: &str, user: &User, csrf: Option<String>) -> Self {
        Self { user: name.to_string(), role: user.role, clients: user.clients.clone(), csrf }
    }
}

struct LoggedIn {
    session: String,
    me: Me,
}

/// `POST /api/login {user, password}`: sets the session cookie, and answers [`Me`] with the
/// session's CSRF token.
pub(super) async fn login(
    axum::extract::State(state): axum::extract::State<super::State>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Login>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(Login { user, password }) = body.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let ip = state.auth.client_ip(peer, &headers);
    match state.auth.login(&user, password, ip).await {
        Ok(LoggedIn { session, me }) => {
            info!(user, %ip, "API login");
            Ok(([(SET_COOKIE, state.auth.cookie(&session))], Json(me)).into_response())
        }
        Err(refusal) => Err(refuse(refusal, Some(&user), ip)),
    }
}

/// `POST /api/logout`: ends the caller's session, if they have one, and clears its cookie.
pub(super) async fn logout(
    axum::extract::State(state): axum::extract::State<super::State>,
    caller: Caller,
) -> Response {
    if let Some(session) = &caller.session {
        state.auth.end_session(&session.id);
    }
    info!(user = caller.user, ip = %caller.ip, "API logout");
    (StatusCode::NO_CONTENT, [(SET_COOKIE, state.auth.cookie(""))]).into_response()
}

/// `GET /api/me`: who the caller is, and with a session its CSRF token.
pub(super) async fn me(caller: Caller) -> Json<Me> {
    let Caller { user, role, clients, session, .. } = caller;
    Json(Me { user, role, clients, csrf: session.map(|session| session.csrf) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use std::sync::Weak;
    use tower::ServiceExt;

    const PEER: &str = "192.0.2.1:5000";

    /// Alice, an operator scoped to FUNDA with password "secret", and bob, an admin.
    fn users() -> Users {
        let mut users = Users::default();
        users.add("alice", Role::Operator, vec!["FUNDA".to_string()], "secret").unwrap();
        users.add("bob", Role::Admin, Vec::new(), "hunter2").unwrap();
        users
    }

    /// A test clock, moved on by hand.
    #[derive(Clone)]
    struct TestClock(Arc<Mutex<Instant>>);

    impl TestClock {
        fn pass(&self, duration: Duration) {
            *self.0.lock().unwrap() += duration;
        }
    }

    fn auth(users: Users, behind_proxy: bool) -> (Arc<Auth>, TestClock) {
        let clock = TestClock(Arc::new(Mutex::new(Instant::now())));
        let now = clock.clone();
        let mut auth = Auth::new(PathBuf::from("users.toml"), users, false, behind_proxy);
        auth.clock = Arc::new(move || *now.0.lock().unwrap());
        (Arc::new(auth), clock)
    }

    fn app(auth: &Arc<Auth>) -> axum::Router {
        super::super::router(super::super::State { service: Weak::new(), auth: auth.clone() })
            .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
    }

    struct Answer {
        status: StatusCode,
        headers: HeaderMap,
        body: serde_json::Value,
    }

    async fn send(app: &axum::Router, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
        let mut request = Request::builder().method(method).uri(path).header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = app.clone().oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body = if body.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&body).unwrap() };
        Answer { status: parts.status, headers: parts.headers, body }
    }

    async fn login(app: &axum::Router, user: &str, password: &str) -> Answer {
        let body = serde_json::json!({ "user": user, "password": password }).to_string();
        send(app, "POST", "/api/login", &[], &body).await
    }

    /// Logs in, returning the `Cookie` header to send and the CSRF token.
    async fn session(app: &axum::Router, user: &str, password: &str) -> (String, String) {
        let answer = login(app, user, password).await;
        assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.body);
        let set = answer.headers[SET_COOKIE].to_str().unwrap();
        let cookie = set.split(';').next().unwrap().to_string();
        (cookie, answer.body["csrf"].as_str().unwrap().to_string())
    }

    /// The routes behind authentication: Tasks 6 to 8 add theirs. Each answers 401 without
    /// credentials; only health and login don't need them.
    const AUTHENTICATED: &[(&str, &str)] = &[("GET", "/api/me"), ("POST", "/api/logout")];

    #[tokio::test]
    async fn every_route_but_health_and_login_needs_a_caller() {
        let (auth, _) = auth(users(), false);
        let app = app(&auth);
        for (method, path) in AUTHENTICATED {
            let answer = send(&app, method, path, &[], "").await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert_eq!(answer.body["error"], Refusal::NoCredentials.message());
        }
        assert_eq!(send(&app, "GET", "/api/health", &[], "").await.status, StatusCode::OK);
        assert_eq!(send(&app, "GET", "/api/nothing", &[], "").await.status, StatusCode::NOT_FOUND);
    }

    /// A password logs in: the cookie is HttpOnly, SameSite=Strict, for /api, and the session
    /// authenticates. A wrong password, or an unknown user, doesn't.
    #[tokio::test]
    async fn a_password_authenticates() {
        let (auth, _) = auth(users(), false);
        let app = app(&auth);
        let answer = login(&app, "alice", "secret").await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body["user"], "alice");
        assert_eq!(answer.body["role"], "operator");
        assert_eq!(answer.body["clients"], serde_json::json!(["FUNDA"]));
        let set = answer.headers[SET_COOKIE].to_str().unwrap();
        assert!(set.starts_with("jetbridge_session="), "{set}");
        assert!(set.ends_with("; HttpOnly; SameSite=Strict; Path=/api"), "{set}");

        let (cookie, csrf) = session(&app, "alice", "secret").await;
        let me = send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await;
        assert_eq!(me.status, StatusCode::OK);
        assert_eq!(me.body["user"], "alice");
        assert_eq!(me.body["csrf"], csrf.as_str());

        for (user, password) in [("alice", "Secret"), ("carol", "secret"), (&"x".repeat(65) as &str, "secret")] {
            let answer = login(&app, user, password).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{user}");
            assert!(!answer.headers.contains_key(SET_COOKIE));
        }
        let answer = send(&app, "GET", "/api/me", &[("cookie", "jetbridge_session=made-up")], "").await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, "POST", "/api/login", &[], "{\"user\":\"alice\"}").await.status, StatusCode::BAD_REQUEST);
    }

    /// Under TLS or behind a proxy, the cookie is Secure.
    #[test]
    fn the_cookie_is_secure_under_tls() {
        let auth = Auth::new(PathBuf::new(), Users::default(), true, true);
        assert!(auth.cookie("x").ends_with("; Path=/api; Secure"));
        assert!(auth.cookie("").ends_with("; Secure; Max-Age=0"));
    }

    /// A token authenticates, and a token revoked by a reload doesn't, from the next request.
    /// A wrong token is refused even with a good cookie.
    #[tokio::test]
    async fn a_token_authenticates_until_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.toml");
        let mut users = users();
        let token = users.add_token("bob").unwrap();
        users.save(&path).unwrap();
        let api = Api {
            listen: "127.0.0.1:0".parse().unwrap(),
            users: path.clone(),
            audit: PathBuf::new(),
            tls: None,
            behind_proxy: false,
        };
        let auth = Arc::new(Auth::load(&api).unwrap());
        let app = app(&auth);
        let bearer = format!("Bearer {token}");
        let me = send(&app, "GET", "/api/me", &[("authorization", &bearer)], "").await;
        assert_eq!(me.status, StatusCode::OK);
        assert_eq!(me.body["user"], "bob");
        assert_eq!(me.body["role"], "admin");
        assert_eq!(me.body.get("csrf"), None);
        // A token needs no CSRF token: a browser doesn't send it by itself.
        assert_eq!(
            send(&app, "POST", "/api/logout", &[("authorization", &bearer)], "").await.status,
            StatusCode::NO_CONTENT
        );

        let (cookie, _) = session(&app, "bob", "hunter2").await;
        let wrong = [("authorization", "Bearer jb_wrong"), ("cookie", cookie.as_str())];
        assert_eq!(send(&app, "GET", "/api/me", &wrong, "").await.status, StatusCode::UNAUTHORIZED);

        // A file that doesn't load keeps the users in use.
        std::fs::write(&path, "[user.bob]\nrole = \"root\"\n").unwrap();
        assert!(auth.reload().unwrap().contains("keeping the users in use"));
        assert_eq!(send(&app, "GET", "/api/me", &[("authorization", &bearer)], "").await.status, StatusCode::OK);

        users.revoke_token("bob", &token).unwrap();
        users.save(&path).unwrap();
        assert_eq!(auth.reload(), None);
        let answer = send(&app, "GET", "/api/me", &[("authorization", &bearer)], "").await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.body["error"], Refusal::BadToken.message());
    }

    /// A session idle for [`SESSION_IDLE`] ends; one used meanwhile doesn't. Logging out ends one.
    #[tokio::test]
    async fn sessions_end_when_idle_or_logged_out() {
        let (auth, clock) = auth(users(), false);
        let app = app(&auth);
        let (cookie, csrf) = session(&app, "alice", "secret").await;
        let me = async || send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status;
        clock.pass(SESSION_IDLE - Duration::from_secs(1));
        assert_eq!(me().await, StatusCode::OK);
        clock.pass(SESSION_IDLE - Duration::from_secs(1));
        assert_eq!(me().await, StatusCode::OK);
        clock.pass(SESSION_IDLE);
        assert_eq!(me().await, StatusCode::UNAUTHORIZED);

        let (cookie, csrf_2) = session(&app, "alice", "secret").await;
        assert_ne!(csrf, csrf_2);
        let answer = send(&app, "POST", "/api/logout", &[("cookie", &cookie), ("x-csrf-token", &csrf_2)], "").await;
        assert_eq!(answer.status, StatusCode::NO_CONTENT);
        assert!(answer.headers[SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status, StatusCode::UNAUTHORIZED);
    }

    /// A user removed, or whose password changed, loses their sessions; a changed role applies at
    /// once.
    #[tokio::test]
    async fn sessions_follow_the_users_file() {
        let users = users();
        let (auth, _) = auth(users.clone(), false);
        let app = app(&auth);
        let (alice, _) = session(&app, "alice", "secret").await;
        let (bob, _) = session(&app, "bob", "hunter2").await;
        let mut changed = users;
        changed.remove("bob");
        changed.add("bob", Role::Admin, Vec::new(), "hunter3").unwrap();
        let changed = toml::to_string(&changed).unwrap().replace("role = \"operator\"", "role = \"viewer\"");
        *auth.users.write().unwrap() = Arc::new(changed.parse().unwrap());
        let answer = send(&app, "GET", "/api/me", &[("cookie", &alice)], "").await;
        assert_eq!(answer.body["role"], "viewer");
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &bob)], "").await.status, StatusCode::UNAUTHORIZED);
        *auth.users.write().unwrap() = Arc::new(Users::default());
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &alice)], "").await.status, StatusCode::UNAUTHORIZED);
    }

    /// With the cookie, a request other than a GET needs the session's CSRF token: missing or
    /// wrong is 403, and the session lives on.
    #[tokio::test]
    async fn csrf_missing_or_wrong_is_forbidden() {
        let (auth, _) = auth(users(), false);
        let app = app(&auth);
        let (cookie, csrf) = session(&app, "alice", "secret").await;
        let (_, other) = session(&app, "alice", "secret").await;
        for headers in [
            vec![("cookie", cookie.as_str())],
            vec![("cookie", &cookie), ("x-csrf-token", &other)],
            vec![("cookie", &cookie), ("x-csrf-token", "")],
        ] {
            let answer = send(&app, "POST", "/api/logout", &headers, "").await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{headers:?}");
            assert_eq!(answer.body["error"], Refusal::Csrf.message());
        }
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status, StatusCode::OK);
        let headers = [("cookie", cookie.as_str()), ("x-csrf-token", &csrf)];
        assert_eq!(send(&app, "POST", "/api/logout", &headers, "").await.status, StatusCode::NO_CONTENT);
    }

    /// Five failures for a name refuse its logins, even with the right password, from any
    /// address, until a minute after the first; then it logs in again.
    #[tokio::test]
    async fn failed_logins_are_limited_per_user() {
        let (auth, clock) = auth(users(), false);
        let app = app(&auth);
        for _ in 0..LOGIN_FAILURES {
            assert_eq!(login(&app, "alice", "guess").await.status, StatusCode::UNAUTHORIZED);
        }
        let answer = login(&app, "alice", "secret").await;
        assert_eq!(answer.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(answer.body["error"], Refusal::Limited.message());
        // Another address is limited for alice too; bob isn't, from another address.
        let elsewhere = "198.51.100.7".parse().unwrap();
        assert_eq!(auth.login("alice", "secret".into(), elsewhere).await.err(), Some(Refusal::Limited));
        assert!(auth.login("bob", "hunter2".into(), "198.51.100.7".parse().unwrap()).await.is_ok());
        clock.pass(LOGIN_WINDOW - Duration::from_secs(1));
        assert_eq!(login(&app, "alice", "secret").await.status, StatusCode::TOO_MANY_REQUESTS);
        clock.pass(Duration::from_secs(1));
        assert_eq!(login(&app, "alice", "secret").await.status, StatusCode::OK);
    }

    /// A burst of guesses at once gets no more tries than the limit, give or take those already
    /// being checked.
    #[tokio::test]
    async fn a_burst_of_guesses_is_limited_too() {
        let (auth, _) = auth(users(), false);
        let ip = IpAddr::from([192, 0, 2, 1]);
        let mut guesses = tokio::task::JoinSet::new();
        for _ in 0..50 {
            let auth = auth.clone();
            guesses.spawn(async move { auth.login("alice", "guess".into(), ip).await.err() });
        }
        let refusals = guesses.join_all().await;
        let tried = refusals.iter().filter(|r| **r == Some(Refusal::BadPassword)).count();
        let limit = usize::try_from(LOGIN_FAILURES).unwrap() + MAX_PASSWORD_CHECKS;
        assert!(tried <= limit, "{tried} guesses checked");
        assert!(refusals.iter().all(|r| matches!(r, Some(Refusal::BadPassword | Refusal::Limited))));
    }

    /// Five failures from an address refuse its logins, whatever the name: unknown names count.
    #[tokio::test]
    async fn failed_logins_are_limited_per_address() {
        let (auth, clock) = auth(users(), false);
        let app = app(&auth);
        for i in 0..LOGIN_FAILURES {
            assert_eq!(login(&app, &format!("nobody{i}"), "guess").await.status, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(login(&app, "bob", "hunter2").await.status, StatusCode::TOO_MANY_REQUESTS);
        clock.pass(LOGIN_WINDOW);
        assert_eq!(login(&app, "bob", "hunter2").await.status, StatusCode::OK);
    }

    /// When the limit is full, expired windows go; if none have, new names and addresses are
    /// refused.
    #[tokio::test]
    async fn the_login_limit_is_bounded() {
        let (auth, clock) = auth(users(), false);
        let ip = |i: usize| IpAddr::from(std::net::Ipv6Addr::from(u128::try_from(i).unwrap()));
        {
            let mut failures = auth.failures.lock().unwrap();
            let start = auth.now();
            for i in 0..MAX_LOGIN_WINDOWS {
                failures.insert(LimitKey::Ip(ip(i)), Window { start, failures: 1 });
            }
        }
        assert_eq!(auth.check_limit("bob", ip(MAX_LOGIN_WINDOWS)), Err(Refusal::Limited));
        assert_eq!(auth.check_limit("bob", ip(0)), Ok(()));
        clock.pass(LOGIN_WINDOW);
        assert_eq!(auth.check_limit("bob", ip(MAX_LOGIN_WINDOWS)), Ok(()));
        assert!(auth.failures.lock().unwrap().is_empty());
    }

    /// When the sessions are full, the least recently used goes.
    #[tokio::test]
    async fn the_least_recently_used_session_goes_when_full() {
        let users = users();
        let (auth, clock) = auth(users.clone(), false);
        let user = users.get("bob").unwrap().clone();
        for i in 0..MAX_SESSIONS {
            clock.pass(Duration::from_millis(1));
            let session = Session {
                user: "bob".into(),
                password: user.password.clone(),
                csrf: String::new(),
                last_seen: auth.now(),
            };
            auth.add_session(i.to_string(), session);
        }
        let headers = |id: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(COOKIE, format!("{SESSION_COOKIE}={id}").parse().unwrap());
            headers
        };
        let ip = IpAddr::from([127, 0, 0, 1]);
        clock.pass(Duration::from_millis(1));
        assert!(auth.authenticate(&Method::GET, &headers("0"), ip).is_ok());
        let app = app(&auth);
        session(&app, "bob", "hunter2").await;
        assert_eq!(auth.sessions.lock().unwrap().len(), MAX_SESSIONS);
        assert!(auth.authenticate(&Method::GET, &headers("0"), ip).is_ok());
        assert_eq!(auth.authenticate(&Method::GET, &headers("1"), ip).err(), Some(Refusal::SessionEnded));
    }

    /// Behind a proxy, the client is the last X-Forwarded-For hop, else the peer; otherwise
    /// always the peer.
    #[test]
    fn the_clients_address_behind_a_proxy() {
        let peer: SocketAddr = PEER.parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", "10.0.0.1, 10.0.0.2".parse().unwrap());
        headers.append("x-forwarded-for", "203.0.113.9, 2001:db8::1".parse().unwrap());
        let proxied = Auth::new(PathBuf::new(), Users::default(), true, true);
        let direct = Auth::new(PathBuf::new(), Users::default(), false, false);
        assert_eq!(proxied.client_ip(peer, &headers), "2001:db8::1".parse::<IpAddr>().unwrap());
        assert_eq!(direct.client_ip(peer, &headers), peer.ip());
        assert_eq!(proxied.client_ip(peer, &HeaderMap::new()), peer.ip());
        let garbage = HeaderMap::from_iter([("x-forwarded-for".parse().unwrap(), "unknown".parse().unwrap())]);
        assert_eq!(proxied.client_ip(peer, &garbage), peer.ip());
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:1".parse().unwrap();
        assert_eq!(direct.client_ip(mapped, &headers), peer.ip());
    }

    /// Roles are ordered; a scoped caller acts on its CompIDs' clients only, and never on all.
    #[test]
    fn roles_and_scopes() {
        let users = users();
        let ip = IpAddr::from([127, 0, 0, 1]);
        let alice = Caller::new("alice", users.get("alice").unwrap(), ip, None);
        let bob = Caller::new("bob", users.get("bob").unwrap(), ip, None);
        assert!(alice.require(Role::Viewer).is_ok());
        assert!(alice.require(Role::Operator).is_ok());
        assert_eq!(alice.require(Role::Admin).unwrap_err().status, StatusCode::FORBIDDEN);
        assert!(bob.require(Role::Admin).is_ok());

        let funda = ClientId::from("FUNDA");
        let fundb = ClientId::from("FUNDB");
        assert!(alice.may_act_on(&funda));
        assert!(!alice.may_act_on(&fundb));
        assert!(alice.may_act_on("FUNDA"));
        assert!(alice.may_act_on(&Scope::Session(funda)));
        assert!(alice.may_act_on(&Scope::CompId("FUNDA".into())));
        assert!(!alice.may_act_on(&Scope::CompId("FUNDB".into())));
        assert!(!alice.may_act_on(&Scope::All));
        assert!(bob.may_act_on(&Scope::All));
        assert!(bob.may_act_on(&fundb));
    }

    /// An unknown user's login costs a password check too.
    #[test]
    fn the_dummy_hash_is_a_real_one() {
        assert!(dummy_hash().starts_with("$argon2id$"));
        assert!(!verify_password(dummy_hash(), ""));
    }
}
