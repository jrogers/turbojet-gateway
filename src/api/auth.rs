//! Who is calling: logins and their sessions, tokens, the CSRF check, and the login limit.
//!
//! Every route but health and login sits behind [`authenticate`], which puts a [`Caller`] in the
//! request or refuses it, so a handler can't forget to check. A caller is either:
//!
//! - **a token**, `Authorization: Bearer jb_...`, for scripts. A header that's there but wrong is
//!   refused, never passed over for the cookie.
//! - **a session**, from `POST /api/login`: the session cookie (`__Host-jetbridge_session` under
//!   TLS, here or at a proxy; `jetbridge_session` on plain loopback), which a browser sends by
//!   itself, so every request but a GET or HEAD must also carry `X-CSRF-Token`, the session's own
//!   token, which a page on another site can't read. A websocket upgrade can't carry a header a
//!   page sets, so it must come from a page on the API's own host instead: its `Origin` must name
//!   the `Host` (behind a proxy, `X-Forwarded-Host` if sent). A session ends after
//!   [`SESSION_IDLE`] unused or [`SESSION_MAX`] in all. Sessions are kept in memory: a restart
//!   logs everyone out.
//!
//! Each request reads the user afresh from the users file in use, so a reload that removes a user
//! or a token, or changes a role or clients, applies from the next request; a session whose user
//! was removed, or whose password changed, ends.
//!
//! Auth outcomes all pass through [`login`], [`logout`] and [`refuse`]; they're logged there, and
//! recorded in the audit log (a login before its session starts) as [`crate::audit`] says.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, FromRequestParts, Request};
use axum::http::header::{AUTHORIZATION, COOKIE, ORIGIN, SET_COOKIE, UPGRADE};
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
use crate::audit::{Entry, cut_at_char};
use crate::auth::{MAX_NAME_LEN, MAX_PASSWORD_LEN, Role, User, Users, check_name, random, verify_password};
use crate::book::{ClientId, Scope};
use crate::config::Api;

/// The session cookie's name over plain HTTP (on loopback), for `Path=/api`.
pub(crate) const SESSION_COOKIE: &str = "jetbridge_session";

/// The session cookie's name under TLS, here or at a proxy. The `__Host-` prefix makes browsers
/// refuse it unless it's `Secure`, `Path=/` and has no `Domain`, so a sibling subdomain can't
/// plant or shadow it.
pub(crate) const SESSION_COOKIE_SECURE: &str = "__Host-jetbridge_session";

/// The header a session's every request but a GET or HEAD carries its CSRF token in.
const CSRF_HEADER: &str = "x-csrf-token";

/// How long a session lasts unused: an operator's working day, so a console left open over a
/// shift stays logged in, and one left overnight doesn't.
pub(crate) const SESSION_IDLE: Duration = Duration::from_secs(8 * 60 * 60);

/// How long a session lasts however much it's used: a stolen cookie stops working within a day,
/// and an operator logs in once a day at least.
pub(crate) const SESSION_MAX: Duration = Duration::from_secs(24 * 60 * 60);

/// Sessions at once. A handful of operators need a few each; when full, the least recently used
/// goes. Refusing new logins instead would let a script that logs in without logging out lock
/// everyone out of the kill switch until its sessions idle out; evicting costs a forgotten
/// console its session, and the one in use is never the least recently used.
const MAX_SESSIONS: usize = 1024;

/// Failed logins a user name or an address may have in a [`LOGIN_WINDOW`]; then logins for it
/// are refused, 429, until the window passes. An IPv6 address counts as its /64, which one host
/// usually has whole.
const LOGIN_FAILURES: u32 = 5;

/// See [`LOGIN_FAILURES`]. The window starts at its first failure.
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// User names the login limit keeps a window for. Expired windows are dropped when it fills; if
/// it's still full, logins for names not in it are refused until some expire: past this many in a
/// minute, someone is guessing, and refusing beats forgetting who. Names and addresses have a
/// limit each, so a flood of one can't crowd the other out.
const MAX_NAME_WINDOWS: usize = 10_000;

/// Addresses the login limit keeps a window for: as [`MAX_NAME_WINDOWS`], for addresses.
const MAX_ADDRESS_WINDOWS: usize = 10_000;

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
    failures: Mutex<Failures>,
    checks: Semaphore,
    /// Whether the session cookie is `Secure`: served over TLS, here or at a proxy.
    secure: bool,
    /// Whether the client's address is the last `X-Forwarded-For` hop rather than the peer's.
    behind_proxy: bool,
    /// Whether a proxy's missing or unreadable `X-Forwarded-For` has been warned of: once is
    /// enough to say the proxy is misconfigured.
    proxy_warned: AtomicBool,
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
    created: Instant,
    last_seen: Instant,
}

impl Session {
    fn ended(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_seen) >= SESSION_IDLE
            || now.saturating_duration_since(self.created) >= SESSION_MAX
    }
}

/// Failed logins: windows by name, and by address.
#[derive(Debug, Default)]
struct Failures {
    names: HashMap<String, Window>,
    addresses: HashMap<IpAddr, Window>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    failures: u32,
}

/// The longest request target an `authenticate` refusal's audit line keeps, in bytes: the path is
/// the client's to write.
const MAX_AUDIT_TARGET_LEN: usize = 256;

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
    /// A websocket upgrade with the session cookie, from a page on another host or none.
    Origin,
    Limited,
}

impl Refusal {
    fn status(self) -> StatusCode {
        match self {
            Self::NoCredentials | Self::BadPassword | Self::BadToken | Self::SessionEnded => StatusCode::UNAUTHORIZED,
            Self::Csrf | Self::Origin => StatusCode::FORBIDDEN,
            Self::Limited => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    pub(super) fn message(self) -> &'static str {
        match self {
            Self::NoCredentials => "log in, or send Authorization: Bearer <token>",
            Self::BadPassword => "wrong user or password",
            Self::BadToken => "unknown or revoked token",
            Self::SessionEnded => "the session has ended: log in again",
            Self::Csrf => "missing or wrong X-CSRF-Token",
            Self::Origin => "a websocket with the session cookie must come from this host's pages",
            Self::Limited => "too many failed logins: try again in a minute",
        }
    }

    /// Whether the audit log records it: not a request with no credentials, nor one whose
    /// session has ended, which a console left open sends every few seconds and which try no
    /// credential. See [`crate::audit`].
    fn audited(self) -> bool {
        !matches!(self, Self::NoCredentials | Self::SessionEnded)
    }
}

/// A refusal, and the user it was for if the request named one validly (a session's user, whose
/// CSRF token was wrong).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused(Refusal, Option<String>);

impl From<Refusal> for Refused {
    fn from(refusal: Refusal) -> Self {
        Self(refusal, None)
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
            failures: Mutex::new(Failures::default()),
            checks: Semaphore::new(MAX_PASSWORD_CHECKS),
            secure,
            behind_proxy,
            proxy_warned: AtomicBool::new(false),
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

    /// The sessions. A panic while they were held can't have left one half-changed: each change
    /// is a single insert, remove or field write.
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The login limit's windows; as with [`sessions`](Self::sessions), never half-changed.
    fn failures(&self) -> MutexGuard<'_, Failures> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The client's address: the peer's, or behind a proxy the last `X-Forwarded-For` hop, the
    /// one the proxy added (those before it are the client's to write), with or without a port.
    /// A proxy that sends none, or garbage, is the client, and is warned of once.
    pub(crate) fn client_ip(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        let forwarded = || {
            let last = headers.get_all("x-forwarded-for").iter().next_back()?.to_str().ok()?;
            let hop = last.rsplit(',').next()?.trim();
            hop.parse::<IpAddr>().ok().or_else(|| hop.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
        };
        let ip = if self.behind_proxy { forwarded() } else { None };
        if self.behind_proxy && ip.is_none() && !self.proxy_warned.swap(true, Ordering::Relaxed) {
            warn!(
                %peer,
                "behind_proxy, but a request has no X-Forwarded-For with an address: the proxy itself counts as \
                 the client (set the proxy to send it); not warning again"
            );
        }
        ip.unwrap_or(peer.ip()).to_canonical()
    }

    /// Checks `password` for user `name`, from `ip`, returning the user, whom
    /// [`start_session`](Self::start_session) then logs in. An unknown user costs a password check
    /// too, so how long a refusal takes doesn't say whether the user exists.
    async fn verify(&self, name: &str, password: String, ip: IpAddr) -> Result<User, Refusal> {
        if name.len() > MAX_NAME_LEN || password.len() > MAX_PASSWORD_LEN {
            // No user has such a name or password: nothing to guess, and nothing worth a window.
            return Err(Refusal::BadPassword);
        }
        self.check_limit(name, ip)?;
        let users = self.users();
        let user = users.get(name);
        let hash = user.map(|user| user.password.clone());
        let correct = {
            let _turn = self.checks.acquire().await.expect("the semaphore is never closed");
            // Again, as failures counted while this waited its turn may have reached the limit:
            // else a burst of guesses all passing the first check would each get a try. With
            // it, a burst gets at most LOGIN_FAILURES + MAX_PASSWORD_CHECKS - 1 tries: the
            // limit's less one, plus those already being checked.
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
        Ok(user.clone())
    }

    /// Starts a session for `user`, called `name`, as verified.
    fn start_session(&self, name: &str, user: &User) -> LoggedIn {
        let session = Base64UrlUnpadded::encode_string(&random::<32>());
        let csrf = Base64UrlUnpadded::encode_string(&random::<32>());
        let now = self.now();
        self.add_session(
            session.clone(),
            Session {
                user: name.to_string(),
                password: user.password.clone(),
                csrf: csrf.clone(),
                created: now,
                last_seen: now,
            },
        );
        LoggedIn { session, me: Me::new(name, user, Some(csrf)) }
    }

    /// Refuses a login for `name` or `ip` if either has had too many failures lately, or if the
    /// limit is full of other names, or other addresses.
    fn check_limit(&self, name: &str, ip: IpAddr) -> Result<(), Refusal> {
        let now = self.now();
        let mut failures = self.failures();
        let checked = check_window(&mut failures.names, name, MAX_NAME_WINDOWS, now)
            .and_then(|()| check_window(&mut failures.addresses, &limit_address(ip), MAX_ADDRESS_WINDOWS, now));
        if checked == Err(Limited::Full) {
            warn!(%ip, "the login limit is full: refusing logins from new names or addresses");
        }
        checked.map_err(|_| Refusal::Limited)
    }

    fn count_failure(&self, name: &str, ip: IpAddr) {
        let now = self.now();
        let mut failures = self.failures();
        count_window(&mut failures.names, name.to_string(), MAX_NAME_WINDOWS, now);
        count_window(&mut failures.addresses, limit_address(ip), MAX_ADDRESS_WINDOWS, now);
    }

    fn add_session(&self, id: String, session: Session) {
        let now = self.now();
        let mut sessions = self.sessions();
        if sessions.len() >= MAX_SESSIONS {
            sessions.retain(|_, session| !session.ended(now));
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
    fn authenticate(&self, method: &Method, headers: &HeaderMap, ip: IpAddr) -> Result<Caller, Refused> {
        self.check(method, headers, ip, true)
    }

    /// The caller as it is now, if the GET with `headers` that `ip` upgraded to a websocket still
    /// authenticates: its session hasn't ended, or its token hasn't been revoked. Its role and
    /// clients are the user's now, which a reload may have changed. Unlike a request, it doesn't
    /// count as using the session, so a socket left open doesn't keep its session from idling out.
    pub(super) fn recheck(&self, headers: &HeaderMap, ip: IpAddr) -> Result<Caller, Refusal> {
        self.check(&Method::GET, headers, ip, false).map_err(|Refused(refusal, _)| refusal)
    }

    /// [`authenticate`](Self::authenticate), counting the request as the session's use if `used`.
    fn check(&self, method: &Method, headers: &HeaderMap, ip: IpAddr, used: bool) -> Result<Caller, Refused> {
        let users = self.users();
        if let Some(value) = headers.get(AUTHORIZATION) {
            // The scheme is case-insensitive (RFC 9110).
            let (scheme, token) = value.to_str().ok().and_then(|v| v.split_once(' ')).ok_or(Refusal::BadToken)?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return Err(Refusal::BadToken.into());
            }
            let (name, user) = users.by_token(token.trim()).ok_or(Refusal::BadToken)?;
            return Ok(Caller::new(name, user, ip, None));
        }
        let id = session_cookie(headers, self.cookie_name()).ok_or(Refusal::NoCredentials)?;
        let now = self.now();
        let mut sessions = self.sessions();
        let session = sessions.get_mut(id).ok_or(Refusal::SessionEnded)?;
        let user = users.get(&session.user).filter(|user| user.password == session.password);
        let (Some(user), false) = (user, session.ended(now)) else {
            sessions.remove(id);
            return Err(Refusal::SessionEnded.into());
        };
        if !matches!(*method, Method::GET | Method::HEAD) {
            let sent = headers.get(CSRF_HEADER).map_or(&[][..], |value| value.as_bytes());
            if !bool::from(sent.ct_eq(session.csrf.as_bytes())) {
                return Err(Refused(Refusal::Csrf, Some(session.user.clone())));
            }
        }
        if is_websocket(headers) && !self.same_origin(headers) {
            return Err(Refused(Refusal::Origin, Some(session.user.clone())));
        }
        if used {
            session.last_seen = now;
        }
        let reference = SessionRef { id: id.to_string(), csrf: session.csrf.clone() };
        Ok(Caller::new(&session.user, user, ip, Some(reference)))
    }

    /// Whether the request's `Origin` is the host it was sent to: the `Host`, or behind a proxy
    /// the last `X-Forwarded-Host` (the proxy's), if sent. Hosts compare without case; a port must
    /// match as written, so a browser's default port left out of one but not the other refuses.
    /// With a `Secure` cookie (TLS here or at a proxy) the page must be served over `https`, so a
    /// plain-HTTP page on the same name, spoofed on the network, can't open the socket.
    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let last = |name| headers.get_all(name).iter().next_back().and_then(|value| value.to_str().ok());
        let forwarded = if self.behind_proxy { last("x-forwarded-host") } else { None };
        let host = forwarded.and_then(|hosts| hosts.rsplit(',').next()).or_else(|| last("host"));
        let origin = headers.get(ORIGIN).and_then(|value| value.to_str().ok());
        // `scheme://host[:port]`; a page with no origin to tell sends `null`, which has no `://`.
        let authority = origin
            .and_then(|origin| origin.split_once("://"))
            .filter(|(scheme, _)| !self.secure || scheme.eq_ignore_ascii_case("https"))
            .map(|(_, authority)| authority);
        matches!((authority, host), (Some(authority), Some(host)) if authority.eq_ignore_ascii_case(host.trim()))
    }

    fn end_session(&self, id: &str) {
        self.sessions().remove(id);
    }

    fn cookie_name(&self) -> &'static str {
        if self.secure { SESSION_COOKIE_SECURE } else { SESSION_COOKIE }
    }

    /// The `Set-Cookie` value for session `id`; an empty `id` clears the cookie.
    fn cookie(&self, id: &str) -> String {
        let path = if self.secure { "Path=/; Secure" } else { "Path=/api" };
        let expire = if id.is_empty() { "; Max-Age=0" } else { "" };
        format!("{}={id}; HttpOnly; SameSite=Strict; {path}{expire}", self.cookie_name())
    }
}

impl Window {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= LOGIN_WINDOW
    }
}

/// Why [`check_window`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limited {
    /// The key has had its failures.
    Key,
    /// The key has no window, and there's no room for one.
    Full,
}

/// Refuses `key` if its window has had [`LOGIN_FAILURES`], or if it has none and `windows` is full
/// at `max` even with the expired ones dropped.
fn check_window<K, Q>(windows: &mut HashMap<K, Window>, key: &Q, max: usize, now: Instant) -> Result<(), Limited>
where
    K: Hash + Eq + std::borrow::Borrow<Q>,
    Q: Hash + Eq + ?Sized,
{
    if let Some(window) = windows.get(key) {
        let limited = !window.expired(now) && window.failures >= LOGIN_FAILURES;
        return if limited { Err(Limited::Key) } else { Ok(()) };
    }
    if windows.len() >= max {
        windows.retain(|_, window| !window.expired(now));
    }
    if windows.len() >= max { Err(Limited::Full) } else { Ok(()) }
}

/// Counts a failure for `key`, starting a window if it has none and there's room: if there
/// isn't, its next login is refused by [`check_window`] anyway.
fn count_window<K: Hash + Eq>(windows: &mut HashMap<K, Window>, key: K, max: usize, now: Instant) {
    let full = windows.len() >= max;
    match windows.get_mut(&key) {
        Some(window) if !window.expired(now) => window.failures += 1,
        Some(window) => *window = Window { start: now, failures: 1 },
        None if full => {}
        None => {
            windows.insert(key, Window { start: now, failures: 1 });
        }
    }
    debug_assert!(windows.len() <= max);
}

/// The address the login limit counts `ip` as: an IPv6 address as its /64.
fn limit_address(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let [a, b, c, d, ..] = v6.segments();
            IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
    }
}

/// A hash no password matches in practice, checked for unknown users so they cost what a wrong
/// password does. Made once, with the cost [`hash_password`](crate::auth::hash_password) gives
/// users.
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| crate::auth::hash_password(&Base64UrlUnpadded::encode_string(&random::<32>())))
}

/// Whether the request asks to upgrade to a websocket.
fn is_websocket(headers: &HeaderMap) -> bool {
    headers.get(UPGRADE).is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

/// The value of the session cookie `name`, if the request has one.
fn session_cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|cookie| cookie.trim().strip_prefix(name)?.strip_prefix('='))
}

/// Who is calling, for handlers: take it as an argument. Only routes behind [`authenticate`]
/// have one; anywhere else, taking it answers 500, never an unauthenticated call.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    pub(crate) user: String,
    pub(crate) role: Role,
    /// The CompIDs the caller may act on; `None`, all.
    pub(crate) clients: Option<Vec<String>>,
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
    pub(crate) fn require(&self, role: Role) -> Result<(), ApiError> {
        if self.role >= role {
            Ok(())
        } else {
            Err(ApiError::new(StatusCode::FORBIDDEN, format!("needs the {role} role; {} is {}", self.user, self.role)))
        }
    }

    /// The CompIDs whose clients the caller may see; `None`, every client's. As it acts, so it
    /// reads: a read naming a client outside these is refused, one naming none narrowed to them.
    pub(crate) fn view_scope(&self) -> Option<&[String]> {
        self.clients.as_deref()
    }

    /// Whether the caller may act on `target`: an unscoped caller on anything, a scoped one on
    /// its own CompIDs' clients only (so never on [`Scope::All`]).
    pub(crate) fn may_act_on<T: Target + ?Sized>(&self, target: &T) -> bool {
        match &self.clients {
            None => true,
            Some(clients) => target.comp_id().is_some_and(|comp_id| clients.iter().any(|c| c == comp_id)),
        }
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
        Err(Refused(refusal, user)) => {
            let error = refuse(refusal, user.as_deref(), ip);
            if !refusal.audited() {
                return error.into_response();
            }
            let mut target = format!("{} {}", request.method(), request.uri().path());
            cut_at_char(&mut target, MAX_AUDIT_TARGET_LEN);
            let entry = Entry::new(user, None, ip, "authenticate", Some(target));
            state.refused(entry, error).await.into_response()
        }
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
    /// `null`: all.
    clients: Option<Vec<String>>,
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
    match state.auth.verify(&user, password, ip).await {
        Ok(found) => {
            let entry = Entry::new(Some(user.clone()), Some(found.role), ip, "login", None);
            let auth = state.auth.clone();
            // The line before the session: no line, no session.
            let start = async move { Ok((auth.start_session(&user, &found), None)) };
            let LoggedIn { session, me } = super::run_audited(state.audit.clone(), entry, start).await?;
            info!(user = me.user, %ip, "API login");
            Ok(([(SET_COOKIE, state.auth.cookie(&session))], Json(me)).into_response())
        }
        Err(refusal) => {
            // A name no user could have is the client's to write anything in: not into the log.
            let error = refuse(refusal, Some(if check_name(&user).is_ok() { &user } else { "<invalid>" }), ip);
            // Nor one no user has into the audit log, whatever was typed there.
            let known = state.auth.users().get(&user).is_some();
            let name = if known { user } else { "<unknown>".to_string() };
            Err(state.refused(Entry::new(Some(name), None, ip, "login", None), error).await)
        }
    }
}

/// `POST /api/logout`: ends the caller's session, if they have one, and clears its cookie.
pub(super) async fn logout(
    axum::extract::State(state): axum::extract::State<super::State>,
    caller: Caller,
) -> Result<Response, ApiError> {
    // A token caller is recorded too: they asked, though there's no session to end.
    let target = if caller.session.is_some() { "session" } else { "token" };
    let entry = super::entry_for(&caller, "logout", Some(target.to_string()));
    let (auth, session) = (state.auth.clone(), caller.session.clone());
    let end = async move {
        if let Some(session) = session {
            auth.end_session(&session.id);
        }
        Ok(((), None))
    };
    super::run_audited(state.audit.clone(), entry, end).await?;
    info!(user = caller.user, ip = %caller.ip, "API logout");
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, state.auth.cookie(""))]).into_response())
}

/// `GET /api/me`: who the caller is, and with a session its CSRF token.
pub(super) async fn me(caller: Caller) -> Json<Me> {
    let Caller { user, role, clients, session, .. } = caller;
    Json(Me { user, role, clients, csrf: session.map(|session| session.csrf) })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Answer, send};
    use super::*;
    use axum::extract::connect_info::MockConnectInfo;

    const PEER: &str = "192.0.2.1:5000";

    /// Alice, an operator scoped to FUNDA with password "alice secret", and bob, an admin.
    fn users() -> Users {
        let mut users = Users::default();
        users.add("alice", Role::Operator, vec!["FUNDA".to_string()], "alice secret").unwrap();
        users.add("bob", Role::Admin, Vec::new(), "hunter2hunter2").unwrap();
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

    /// The API's routes, and the directory of its audit log.
    struct App {
        router: axum::Router,
        dir: tempfile::TempDir,
    }

    impl std::ops::Deref for App {
        type Target = axum::Router;

        fn deref(&self) -> &axum::Router {
            &self.router
        }
    }

    impl App {
        /// The audit log's lines so far.
        fn audit(&self) -> Vec<serde_json::Value> {
            let text = crate::audit::tests::text(self.dir.path());
            text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
        }
    }

    fn app(auth: &Arc<Auth>) -> App {
        let (state, dir) = super::super::tests::state(auth.clone());
        let router = super::super::router(state).layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()));
        App { router, dir }
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

    /// The routes behind authentication, each with a method it takes. Each answers 401 without
    /// credentials; only health and login don't need them.
    const AUTHENTICATED: &[(&str, &str)] = &[
        ("GET", "/api/me"),
        ("POST", "/api/logout"),
        ("GET", "/api/audit"),
        ("GET", "/api/sessions"),
        ("GET", "/api/orders"),
        ("GET", "/api/orders/JB1"),
        ("GET", "/api/orders/JB1/messages"),
        ("GET", "/api/messages?since=2026-10-08T00:00:00Z"),
        ("GET", "/api/risk"),
        ("GET", "/api/risk/limits"),
        ("GET", "/api/routes"),
        ("GET", "/api/routes/test?client=FUNDA&symbol=AAPL"),
        ("GET", "/api/live"),
        ("POST", "/api/sessions/FIX.4.4:JETBRIDGE-%3EFUNDA/pause"),
        ("POST", "/api/sessions/FIX.4.4:JETBRIDGE-%3EFUNDA/resume"),
        ("POST", "/api/sessions/FIX.4.4:JETBRIDGE-%3EFUNDA/resend"),
        ("POST", "/api/sessions/FIX.4.4:JETBRIDGE-%3EFUNDA/sequence"),
        ("POST", "/api/sessions/FIX.4.4:JETBRIDGE-%3EFUNDA/reset"),
        ("POST", "/api/orders/JB1/cancel"),
        ("POST", "/api/orders/JB1/status"),
        ("POST", "/api/orders/JB1/resolve"),
        ("POST", "/api/risk/block"),
        ("POST", "/api/risk/unblock"),
        ("POST", "/api/risk/cancel-all"),
        ("PATCH", "/api/risk/limits/defaults"),
        ("PATCH", "/api/risk/limits/client/FUNDA"),
        ("DELETE", "/api/risk/limits/client/FUNDA"),
        ("POST", "/api/reload"),
    ];

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
        let answer = login(&app, "alice", "alice secret").await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body["user"], "alice");
        assert_eq!(answer.body["role"], "operator");
        assert_eq!(answer.body["clients"], serde_json::json!(["FUNDA"]));
        let set = answer.headers[SET_COOKIE].to_str().unwrap();
        assert!(set.starts_with("jetbridge_session="), "{set}");
        assert!(set.ends_with("; HttpOnly; SameSite=Strict; Path=/api"), "{set}");
        // The scheme of Authorization is case-insensitive: a session's cookie is no fallback.
        let lower = [("authorization", "bearer jb_wrong")];
        assert_eq!(send(&app, "GET", "/api/me", &lower, "").await.body["error"], Refusal::BadToken.message());

        let (cookie, csrf) = session(&app, "alice", "alice secret").await;
        let me = send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await;
        assert_eq!(me.status, StatusCode::OK);
        assert_eq!(me.body["user"], "alice");
        assert_eq!(me.body["csrf"], csrf.as_str());

        let long_name = "x".repeat(MAX_NAME_LEN + 1);
        let long_password = "x".repeat(MAX_PASSWORD_LEN + 1);
        let tries = [
            ("alice", "Alice secret"),
            ("carol", "alice secret"),
            (&long_name, "alice secret"),
            ("alice", &long_password),
        ];
        for (user, password) in tries {
            let answer = login(&app, user, password).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{user}");
            assert!(!answer.headers.contains_key(SET_COOKIE));
        }
        let answer = send(&app, "GET", "/api/me", &[("cookie", "jetbridge_session=made-up")], "").await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, "POST", "/api/login", &[], "{\"user\":\"alice\"}").await.status, StatusCode::BAD_REQUEST);
    }

    /// Under TLS or behind a proxy, the cookie is Secure, for the whole host, with the `__Host-`
    /// prefix; and it's that name the gateway reads.
    #[tokio::test]
    async fn the_cookie_is_secure_under_tls() {
        let (auth, _) = auth(users(), false);
        let mut auth = Arc::into_inner(auth).unwrap();
        auth.secure = true;
        let auth = Arc::new(auth);
        assert_eq!(auth.cookie("x"), "__Host-jetbridge_session=x; HttpOnly; SameSite=Strict; Path=/; Secure");
        assert!(auth.cookie("").ends_with("; Secure; Max-Age=0"));
        let app = app(&auth);
        let (cookie, _) = session(&app, "bob", "hunter2hunter2").await;
        assert!(cookie.starts_with("__Host-jetbridge_session="));
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status, StatusCode::OK);
        let plain = cookie.replace("__Host-", "");
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &plain)], "").await.status, StatusCode::UNAUTHORIZED);
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
            audit_dir: PathBuf::new(),
            audit_retention: None,
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

        let (cookie, _) = session(&app, "bob", "hunter2hunter2").await;
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
        let (cookie, csrf) = session(&app, "alice", "alice secret").await;
        let me = async || send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status;
        clock.pass(SESSION_IDLE - Duration::from_secs(1));
        assert_eq!(me().await, StatusCode::OK);
        clock.pass(SESSION_IDLE - Duration::from_secs(1));
        assert_eq!(me().await, StatusCode::OK);
        clock.pass(SESSION_IDLE);
        assert_eq!(me().await, StatusCode::UNAUTHORIZED);

        // However much it's used, a session ends SESSION_MAX after it began.
        let (cookie, _) = session(&app, "alice", "alice secret").await;
        let me = async || send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status;
        for _ in 0..3 {
            clock.pass(SESSION_MAX / 4);
            assert_eq!(me().await, StatusCode::OK);
        }
        clock.pass(SESSION_MAX / 4);
        assert_eq!(me().await, StatusCode::UNAUTHORIZED);

        let (cookie, csrf_2) = session(&app, "alice", "alice secret").await;
        assert_ne!(csrf, csrf_2);
        let answer = send(&app, "POST", "/api/logout", &[("cookie", &cookie), ("x-csrf-token", &csrf_2)], "").await;
        assert_eq!(answer.status, StatusCode::NO_CONTENT);
        assert!(answer.headers[SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status, StatusCode::UNAUTHORIZED);
    }

    /// A live socket's recheck doesn't count as using its session, which idles out regardless.
    #[tokio::test]
    async fn a_recheck_doesnt_keep_a_session() {
        let (auth, clock) = auth(users(), false);
        let app = app(&auth);
        let (cookie, _) = session(&app, "alice", "alice secret").await;
        let headers = HeaderMap::from_iter([(COOKIE, cookie.parse().unwrap())]);
        let ip = IpAddr::from([127, 0, 0, 1]);
        clock.pass(SESSION_IDLE - Duration::from_secs(1));
        assert!(auth.recheck(&headers, ip).is_ok());
        clock.pass(Duration::from_secs(1));
        assert_eq!(auth.recheck(&headers, ip).err(), Some(Refusal::SessionEnded));
    }

    /// A user removed, or whose password changed, loses their sessions; a changed role applies at
    /// once.
    #[tokio::test]
    async fn sessions_follow_the_users_file() {
        let users = users();
        let (auth, _) = auth(users.clone(), false);
        let app = app(&auth);
        let (alice, _) = session(&app, "alice", "alice secret").await;
        let (bob, _) = session(&app, "bob", "hunter2hunter2").await;
        let mut changed = users;
        changed.remove("bob");
        changed.add("bob", Role::Admin, Vec::new(), "hunter3hunter3").unwrap();
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
        let (cookie, csrf) = session(&app, "alice", "alice secret").await;
        let (_, other) = session(&app, "alice", "alice secret").await;
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
        let answer = login(&app, "alice", "alice secret").await;
        assert_eq!(answer.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(answer.body["error"], Refusal::Limited.message());
        // Another address is limited for alice too; bob isn't, from another address.
        let elsewhere = "198.51.100.7".parse().unwrap();
        assert_eq!(auth.verify("alice", "alice secret".into(), elsewhere).await.err(), Some(Refusal::Limited));
        assert!(auth.verify("bob", "hunter2hunter2".into(), "198.51.100.7".parse().unwrap()).await.is_ok());
        clock.pass(LOGIN_WINDOW - Duration::from_secs(1));
        assert_eq!(login(&app, "alice", "alice secret").await.status, StatusCode::TOO_MANY_REQUESTS);
        clock.pass(Duration::from_secs(1));
        assert_eq!(login(&app, "alice", "alice secret").await.status, StatusCode::OK);
    }

    /// A burst of guesses at once gets at most LOGIN_FAILURES + MAX_PASSWORD_CHECKS - 1 tries:
    /// the limit's less one, plus those already being checked.
    #[tokio::test]
    async fn a_burst_of_guesses_is_limited_too() {
        let (auth, _) = auth(users(), false);
        let ip = IpAddr::from([192, 0, 2, 1]);
        let mut guesses = tokio::task::JoinSet::new();
        for _ in 0..50 {
            let auth = auth.clone();
            guesses.spawn(async move { auth.verify("alice", "guess".into(), ip).await.err() });
        }
        let refusals = guesses.join_all().await;
        let tried = refusals.iter().filter(|r| **r == Some(Refusal::BadPassword)).count();
        let limit = usize::try_from(LOGIN_FAILURES).unwrap() + MAX_PASSWORD_CHECKS - 1;
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
        assert_eq!(login(&app, "bob", "hunter2hunter2").await.status, StatusCode::TOO_MANY_REQUESTS);
        clock.pass(LOGIN_WINDOW);
        assert_eq!(login(&app, "bob", "hunter2hunter2").await.status, StatusCode::OK);
    }

    /// An IPv6 address counts as its /64: a host can't step through its addresses to dodge the
    /// limit. Another /64 isn't limited.
    #[tokio::test]
    async fn ipv6_addresses_are_limited_by_their_64() {
        let (auth, _) = auth(users(), false);
        for i in 0..LOGIN_FAILURES {
            let ip = format!("2001:db8:1:2::{i}").parse().unwrap();
            assert_eq!(auth.verify(&format!("nobody{i}"), "guess".into(), ip).await.err(), Some(Refusal::BadPassword));
        }
        let same = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert_eq!(auth.verify("bob", "hunter2hunter2".into(), same).await.err(), Some(Refusal::Limited));
        assert!(auth.verify("bob", "hunter2hunter2".into(), "2001:db8:1:3::1".parse().unwrap()).await.is_ok());
    }

    /// When the limit's names are full, expired windows go; if none have, new names are refused,
    /// but not names it has, nor any address: names don't crowd out addresses. And the reverse.
    #[tokio::test]
    async fn the_login_limit_is_bounded() {
        let (auth, clock) = auth(users(), false);
        let ip = |i: usize| IpAddr::from([10, 0, u8::try_from(i / 256).unwrap(), u8::try_from(i % 256).unwrap()]);
        let start = auth.now();
        {
            let mut failures = auth.failures();
            for i in 0..MAX_NAME_WINDOWS {
                failures.names.insert(format!("n{i}"), Window { start, failures: 1 });
            }
        }
        assert_eq!(auth.check_limit("bob", ip(0)), Err(Refusal::Limited));
        assert_eq!(auth.check_limit("n0", ip(0)), Ok(()));
        auth.count_failure("bob", ip(0));
        assert_eq!(auth.failures().names.len(), MAX_NAME_WINDOWS);
        assert_eq!(auth.failures().addresses.len(), 1);

        clock.pass(LOGIN_WINDOW);
        assert_eq!(auth.check_limit("bob", ip(0)), Ok(()));
        assert!(auth.failures().names.is_empty());

        let start = auth.now();
        {
            let mut failures = auth.failures();
            for i in 0..MAX_ADDRESS_WINDOWS {
                failures.addresses.insert(ip(i), Window { start, failures: 1 });
            }
        }
        let new = IpAddr::from([192, 0, 2, 1]);
        assert_eq!(auth.check_limit("bob", new), Err(Refusal::Limited));
        assert_eq!(auth.check_limit("bob", ip(1)), Ok(()));
    }

    /// Logins and logouts are recorded, allowed or refused, never with a password, token or
    /// CSRF token; so are refusals that tried a credential (a wrong token, a wrong CSRF token, the
    /// login limit), and not those that tried none (no credentials, a session ended).
    #[tokio::test]
    async fn auth_outcomes_are_audited() {
        let mut users = users();
        let token = users.add_token("bob").unwrap();
        let (auth, _) = auth(users, false);
        let app = app(&auth);
        let (cookie, csrf) = session(&app, "alice", "alice secret").await;
        assert_eq!(login(&app, "alice", "guess").await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(login(&app, &"x".repeat(MAX_NAME_LEN + 1), "guess").await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(login(&app, "carol", "guess").await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, "GET", "/api/me", &[], "").await.status, StatusCode::UNAUTHORIZED);
        let ended = [("cookie", "jetbridge_session=made-up")];
        assert_eq!(send(&app, "GET", "/api/me", &ended, "").await.status, StatusCode::UNAUTHORIZED);
        let wrong = [("authorization", "Bearer jb_wrong")];
        assert_eq!(send(&app, "GET", "/api/me", &wrong, "").await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, "POST", "/api/logout", &[("cookie", &cookie)], "").await.status, StatusCode::FORBIDDEN);
        let headers = [("cookie", cookie.as_str()), ("x-csrf-token", &csrf)];
        assert_eq!(send(&app, "POST", "/api/logout", &headers, "").await.status, StatusCode::NO_CONTENT);
        let bearer = format!("Bearer {token}");
        assert_eq!(send(&app, "GET", "/api/me", &[("authorization", &bearer)], "").await.status, StatusCode::OK);
        let answer = send(&app, "POST", "/api/logout", &[("authorization", &bearer)], "").await;
        assert_eq!(answer.status, StatusCode::NO_CONTENT);
        // The address has two failures already, alice's and carol's.
        for _ in 2..LOGIN_FAILURES {
            login(&app, "alice", "guess").await;
        }
        assert_eq!(login(&app, "alice", "alice secret").await.status, StatusCode::TOO_MANY_REQUESTS);

        let lines = app.audit();
        let line = |i: usize| {
            let line: &serde_json::Value = &lines[i];
            let field = |name: &str| line[name].as_str().unwrap_or("-").to_string();
            [field("user"), field("role"), field("action"), field("target"), field("result")].join(" | ")
        };
        let wrong_password = "alice | - | login | - | refused: wrong user or password";
        let expected = [
            "alice | operator | login | - | allowed",
            wrong_password,
            "<unknown> | - | login | - | refused: wrong user or password",
            "<unknown> | - | login | - | refused: wrong user or password",
            "- | - | authenticate | GET /api/me | refused: unknown or revoked token",
            "alice | - | authenticate | POST /api/logout | refused: missing or wrong X-CSRF-Token",
            "alice | operator | logout | session | allowed",
            "bob | admin | logout | token | allowed",
            wrong_password,
            wrong_password,
            wrong_password,
            "alice | - | login | - | refused: too many failed logins: try again in a minute",
        ];
        assert_eq!((0..lines.len()).map(line).collect::<Vec<_>>(), expected);
        assert_eq!(lines[0]["ip"], "192.0.2.1");
        let text = crate::audit::tests::text(app.dir.path());
        let session = cookie.split_once('=').unwrap().1;
        for secret in ["alice secret", "guess", "carol", &csrf, session, &token] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    /// A refusal's target, the request's path, is cut to its limit at a character's boundary:
    /// a path routed with a parameter may hold any character.
    #[tokio::test]
    async fn a_long_target_is_cut_between_characters() {
        let (auth, _) = auth(users(), false);
        let (state, dir) = super::super::tests::state(auth);
        let app = axum::Router::new()
            .route("/api/x/{id}", axum::routing::get(async || ()))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), authenticate))
            .with_state(state)
            .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()));
        // "GET /api/x/" is 11 bytes, so byte 256 falls inside an "é".
        let path = format!("/api/x/{}", "é".repeat(200));
        let wrong = [("authorization", "Bearer jb_wrong")];
        assert_eq!(send(&app, "GET", &path, &wrong, "").await.status, StatusCode::UNAUTHORIZED);
        let text = crate::audit::tests::text(dir.path());
        let line: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(line["target"], format!("GET {}", &path[..MAX_AUDIT_TARGET_LEN - 5]));
    }

    /// A login whose audit line can't be written answers 503 and starts no session; a logout's
    /// answers 503 and the session lives on.
    #[tokio::test]
    async fn no_audit_line_no_login_or_logout() {
        let (auth, _) = auth(users(), false);
        let app = app(&auth);
        let (cookie, csrf) = session(&app, "alice", "alice secret").await;
        let Some(_unwritable) = crate::audit::tests::make_unwritable(app.dir.path()) else { return };
        let answer = login(&app, "bob", "hunter2hunter2").await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!answer.headers.contains_key(SET_COOKIE));
        assert_eq!(auth.sessions().len(), 1);
        let headers = [("cookie", cookie.as_str()), ("x-csrf-token", &csrf)];
        assert_eq!(send(&app, "POST", "/api/logout", &headers, "").await.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(send(&app, "GET", "/api/me", &[("cookie", &cookie)], "").await.status, StatusCode::OK);
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
                created: auth.now(),
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
        session(&app, "bob", "hunter2hunter2").await;
        assert_eq!(auth.sessions().len(), MAX_SESSIONS);
        assert!(auth.authenticate(&Method::GET, &headers("0"), ip).is_ok());
        assert_eq!(auth.authenticate(&Method::GET, &headers("1"), ip).err(), Some(Refusal::SessionEnded.into()));
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
        let one = |value: &str| HeaderMap::from_iter([("x-forwarded-for".parse().unwrap(), value.parse().unwrap())]);
        assert_eq!(proxied.client_ip(peer, &one("unknown")), peer.ip());
        assert!(proxied.proxy_warned.load(Ordering::Relaxed));
        assert_eq!(proxied.client_ip(peer, &one("203.0.113.9:4711")), IpAddr::from([203, 0, 113, 9]));
        assert_eq!(proxied.client_ip(peer, &one("a, [2001:db8::2]:443")), "2001:db8::2".parse::<IpAddr>().unwrap());
        let mapped: SocketAddr = "[::ffff:192.0.2.1]:1".parse().unwrap();
        assert_eq!(direct.client_ip(mapped, &headers), peer.ip());
    }

    /// The Origin must name the Host, or behind a proxy the last X-Forwarded-Host if there is one;
    /// without a proxy, X-Forwarded-Host is the client's to write and isn't read.
    #[test]
    fn the_origin_must_be_the_host() {
        let proxied = Auth::new(PathBuf::new(), Users::default(), true, true);
        let direct = Auth::new(PathBuf::new(), Users::default(), false, false);
        let headers = |pairs: &[(&'static str, &str)]| {
            HeaderMap::from_iter(pairs.iter().map(|(name, value)| ((*name).parse().unwrap(), value.parse().unwrap())))
        };
        let here = [("host", "gw.example:8443"), ("origin", "https://GW.example:8443")];
        assert!(direct.same_origin(&headers(&here)));
        assert!(proxied.same_origin(&headers(&here)));
        for origin in ["https://gw.example", "https://evil.example:8443", "null", "gw.example:8443"] {
            assert!(!direct.same_origin(&headers(&[("host", "gw.example:8443"), ("origin", origin)])), "{origin}");
        }
        assert!(!direct.same_origin(&headers(&[("host", "gw.example:8443")])));
        // An origin has no path, not even `/`.
        assert!(!direct.same_origin(&headers(&[("host", "gw.example"), ("origin", "https://gw.example/")])));
        let v6 = [("host", "[::1]:8080"), ("origin", "http://[::1]:8080")];
        assert!(direct.same_origin(&headers(&v6)));
        // A Secure cookie's page must be https.
        assert!(!proxied.same_origin(&headers(&v6)));
        let forwarded = [
            ("host", "10.0.0.5:8080"),
            ("x-forwarded-host", "evil.example, gw.example"),
            ("origin", "https://gw.example"),
        ];
        assert!(proxied.same_origin(&headers(&forwarded)));
        assert!(!direct.same_origin(&headers(&forwarded)));
        // A trailing comma leaves an empty last host, which matches nothing; Host isn't tried.
        let trailing = [("host", "gw.example"), ("x-forwarded-host", "gw.example,"), ("origin", "https://gw.example")];
        assert!(!proxied.same_origin(&headers(&trailing)));
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
