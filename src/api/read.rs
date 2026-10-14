//! The API's reads [viewer]: the sessions, the book's orders, risk and routes, as JSON. Every role
//! may read; reads aren't audited. A scoped caller reads its own CompIDs' clients only: a read
//! naming another client is 403, as a write would be, and one naming none is narrowed to them.
//!
//! The orders are the book's: the working ones and each client's done orders of its current day.
//! With a time range, or an order the book doesn't hold, they're the database's: every order kept
//! until pruned.

use std::ops::Range;

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use base64ct::{Base64UrlUnpadded, Encoding};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tracing::error;
use turbojet::fields::CompactString;
use turbojet::{SessionId, SessionStatus};

use super::{ApiError, Caller, State, bad};
use crate::book::{MAX_ORDERS_PER_PAGE, OrderDetail, OrderFilter, OrderId, OrderView, RiskView, ScopeName, Status};
use crate::config::RiskTables;
use crate::edge::Gateway;
use crate::router::{Rule, Target};
use crate::service::Service;
use crate::store::StoredOrders;

/// Orders a page of `GET /api/orders` holds without a `limit`: a screenful.
const ORDERS_PAGE_DEFAULT: usize = 100;

/// The longest time range `GET /api/orders` reads from the database: a page under a filter few
/// orders match may read every order of its range before it's full, so the range bounds that.
const HISTORY_RANGE_MAX: TimeDelta = TimeDelta::days(31);

const _: () = assert!(ORDERS_PAGE_DEFAULT <= MAX_ORDERS_PER_PAGE);

type AxumState = axum::extract::State<State>;

/// A session, a client's or a venue's.
#[derive(Debug, Serialize)]
pub(super) struct SessionView {
    side: SessionSide,
    /// As Turbojet shows it.
    id: String,
    /// A venue's name (`[initiator.NAME]`).
    venue: Option<String>,
    status: SessionState,
    next_incoming: Option<u64>,
    next_outgoing: Option<u64>,
    activity: Option<Activity>,
    paused: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionSide {
    Client,
    Venue,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionState {
    Disconnected,
    /// Bound to a connection, its state not reported.
    Connected,
    LoggingOn,
    LoggedOn,
    LoggingOut,
}

/// When a connected session logged on and last heard and spoke, by its clock.
#[derive(Debug, Serialize)]
struct Activity {
    since: DateTime<Utc>,
    last_received: Option<DateTime<Utc>>,
    last_sent: Option<DateTime<Utc>>,
    /// A ResendRequest of its own is open.
    resending: bool,
}

impl SessionView {
    /// Session `id`, connected with `status` or disconnected (`None`).
    fn new(side: SessionSide, id: &SessionId, paused: bool, status: Option<&SessionStatus>) -> Self {
        let live = status.and_then(|status| status.activity.as_ref());
        let state = match status {
            None => SessionState::Disconnected,
            Some(_) => match live.map(|live| live.state) {
                Some(turbojet::SessionState::LoggingOn) => SessionState::LoggingOn,
                Some(turbojet::SessionState::LoggedOn) => SessionState::LoggedOn,
                Some(turbojet::SessionState::LoggingOut) => SessionState::LoggingOut,
                _ => SessionState::Connected,
            },
        };
        Self {
            side,
            id: id.to_string(),
            venue: None,
            status: state,
            next_incoming: live.map(|live| live.next_incoming),
            next_outgoing: live.map(|live| live.next_outgoing),
            activity: status.map(|status| Activity {
                since: status.since,
                last_received: live.map(|live| live.last_received),
                last_sent: live.map(|live| live.last_sent),
                resending: live.is_some_and(|live| live.resending),
            }),
            paused,
        }
    }
}

/// `GET /api/sessions`: the clients' sessions connected now, by ID, then those paused and not
/// connected, then every venue's, connected or not, by name. A scoped caller's clients only.
pub(super) async fn sessions(
    axum::extract::State(state): AxumState,
    caller: Caller,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    Ok(Json(session_views(&*state.service()?, caller.view_scope())))
}

/// What [`sessions`] answers: the clients' of `comp_ids` alone, if given.
pub(super) fn session_views(service: &Service, comp_ids: Option<&[String]>) -> Vec<SessionView> {
    let shown = |id: &SessionId| super::live::shows_client(comp_ids, id);
    let mut clients = service.client_statuses();
    clients.retain(|status| shown(&status.id));
    clients.sort_by_cached_key(|status| status.id.to_string());
    let mut sessions: Vec<_> = clients
        .iter()
        .map(|status| SessionView::new(SessionSide::Client, &status.id, status.paused, Some(status)))
        .collect();
    // A paused client is logged out, and would otherwise vanish from the list it's resumed from.
    let mut paused = service.client_paused();
    paused.retain(|id| shown(id) && !clients.iter().any(|status| &status.id == id));
    paused.sort_by_cached_key(ToString::to_string);
    sessions.extend(paused.iter().map(|id| SessionView::new(SessionSide::Client, id, true, None)));
    for name in service.venues() {
        // Gone if a reload stopped it since.
        let Some(handle) = service.venue_session(&name) else { continue };
        let status = handle.status();
        let paused = status.as_ref().map_or_else(|| handle.is_paused(), |status| status.paused);
        let mut view = SessionView::new(SessionSide::Venue, handle.id(), paused, status.as_ref());
        view.venue = Some(name);
        sessions.push(view);
    }
    sessions
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OrdersQuery {
    /// A scope: `comp:COMPID` or `session:SESSION` (or `all`).
    client: Option<String>,
    venue: Option<CompactString>,
    symbol: Option<CompactString>,
    status: Option<Status>,
    #[serde(default)]
    working: bool,
    /// Created from (RFC 3339): with it, the orders are the database's.
    from: Option<DateTime<Utc>>,
    /// Created before (RFC 3339), now by default; only with `from`.
    to: Option<DateTime<Utc>>,
    /// The previous page's `next`: a number from the book, a cursor from the database.
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(super) struct OrdersPage {
    orders: Vec<OrderView>,
    /// The `after` for the next page; `None` when this page is short. A full page may be followed
    /// by an empty one.
    next: Option<Next>,
}

/// A page's `next`: the book's are numbers, the database's strings.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Next {
    Id(u64),
    Cursor(String),
}

/// `GET /api/orders?client=&venue=&symbol=&status=&working=&from=&to=&after=&limit=`: the orders
/// matching every filter given, `limit` at a time (by default [`ORDERS_PAGE_DEFAULT`], at most
/// [`MAX_ORDERS_PER_PAGE`]).
///
/// Without `from`, they're the book's, by OrderId. `after` and `next` are then the numeric `id`,
/// not the wire OrderID, so a cursor holds when its order leaves the book.
///
/// With `from` (RFC 3339; `to` is now by default, and needs `from`), they're the database's
/// created in `from..to`, a range of [`HISTORY_RANGE_MAX`] at most, by creation, then `id`: the
/// orders of past days too, until pruned, as last written. `after` and `next` are then an opaque
/// string. A range without `[database]` is 400. Its `client` may name a session not connected now.
///
/// A scoped caller's are its clients' only, and a `client` naming another's is 403: see
/// [`may_see_named`].
pub(super) async fn orders(
    axum::extract::State(state): AxumState,
    caller: Caller,
    query: Result<Query<OrdersQuery>, QueryRejection>,
) -> Result<Json<OrdersPage>, ApiError> {
    let Query(query) = query.map_err(|e| bad(e.body_text()))?;
    let limit = query.limit.unwrap_or(ORDERS_PAGE_DEFAULT);
    if !(1..=MAX_ORDERS_PER_PAGE).contains(&limit) {
        return Err(bad(format!("limit must be 1 to {MAX_ORDERS_PER_PAGE}")));
    }
    let client = match &query.client {
        Some(text) => Some(text.parse::<ScopeName>().map_err(|e| bad(e.to_string()))?),
        None => None,
    };
    if let Some(name) = &client
        && caller.view_scope().is_some()
    {
        may_see_named(&caller, state.service()?.gateway(), name)?;
    }
    let comp_ids = caller.view_scope().map(|comp_ids| comp_ids.iter().map(|id| id.as_str().into()).collect());
    if let Some(range) = range(query.from, query.to, Utc::now())? {
        return stored_orders(&state, query, client, comp_ids, range, limit).await;
    }
    let after = match &query.after {
        Some(text) => Some(OrderId(text.parse().map_err(|_| bad("after isn't a page's next"))?)),
        None => None,
    };
    let service = state.service()?;
    let gateway = service.gateway();
    let client = match &client {
        Some(name) => match gateway.scope(name) {
            Some(scope) => Some(scope),
            // A session the gateway hasn't seen has no orders.
            None => return Ok(Json(OrdersPage { orders: Vec::new(), next: None })),
        },
        None => None,
    };
    let filter = OrderFilter {
        client,
        comp_ids,
        venue: query.venue,
        symbol: query.symbol,
        status: query.status,
        working: query.working,
        after,
        limit: Some(limit),
    };
    let orders = gateway.orders(&filter);
    // Whether more match isn't known without a walk past the page: a full page may be the last.
    let next = if orders.len() == limit { orders.last().map(|order| Next::Id(order.id.0)) } else { None };
    Ok(Json(OrdersPage { orders, next }))
}

/// 403 unless a scoped caller may see each client `name` names: never `all`, and a CompID or
/// session only if it's one of the caller's CompIDs'. A session the gateway knows is checked by
/// its client. One it doesn't, a past session a range may name, is checked by its name, whose
/// target is the client: [`names_target`]. A CompID holding `->`, `/` or `:` can make a name
/// ambiguous, but the orders are filtered by the caller's CompIDs as well, so a name misread
/// still shows none of another client's.
fn may_see_named(caller: &Caller, gateway: &Gateway, name: &ScopeName) -> Result<(), ApiError> {
    let Some(comp_ids) = caller.view_scope() else { return Ok(()) };
    let seen = match name {
        ScopeName::Session(session) => match gateway.client(session) {
            Some(client) => caller.may_act_on(&client),
            None => comp_ids.iter().any(|comp_id| names_target(session, comp_id)),
        },
        ScopeName::CompId(comp_id) => caller.may_act_on(comp_id.as_str()),
        ScopeName::All => false,
    };
    if seen { Ok(()) } else { Err(unseen(caller, &name.to_string())) }
}

/// 403 for a scoped caller reading what isn't theirs: `target`. The read twin of a write's.
pub(super) fn unseen(caller: &Caller, target: &str) -> ApiError {
    let clients = caller.view_scope().unwrap_or_default().join(", ");
    ApiError::new(StatusCode::FORBIDDEN, format!("{} may see only clients {clients}, not {target}", caller.user))
}

/// Whether `session`, a session as Turbojet shows it (`BEGIN:SENDER->TARGET`, a side's SubID and
/// LocationID after a `/` each, a qualifier after a `:`), has `comp_id` as its TargetCompID: the
/// client's, as the acceptor sees it.
pub(super) fn names_target(session: &str, comp_id: &str) -> bool {
    let rest = session.split_once("->").and_then(|(_, target)| target.strip_prefix(comp_id));
    rest.is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', ':']))
}

/// `from..to` of [`orders`], `None` without `from` or `to`, or 400.
fn range(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<Option<Range<DateTime<Utc>>>, ApiError> {
    let (from, to) = match (from, to) {
        (None, None) => return Ok(None),
        (None, Some(_)) => return Err(bad("to needs from")),
        (Some(from), to) => (from, to.unwrap_or(now)),
    };
    if from >= to {
        return Err(bad("from must be before to"));
    }
    if to - from > HISTORY_RANGE_MAX {
        return Err(bad(format!("a range covers {} days at most", HISTORY_RANGE_MAX.num_days())));
    }
    Ok(Some(from..to))
}

/// [`orders`] from the database, created in `range`.
async fn stored_orders(
    state: &State,
    query: OrdersQuery,
    client: Option<ScopeName>,
    comp_ids: Option<Vec<CompactString>>,
    range: Range<DateTime<Utc>>,
    limit: usize,
) -> Result<Json<OrdersPage>, ApiError> {
    // The database first, so a range without one is refused for that whatever its `after`.
    let store = state.service()?.store().cloned().ok_or_else(|| bad("no [database]: only the current day is kept"))?;
    let after = query.after.as_deref().map(decode_after).transpose()?;
    let stored = StoredOrders {
        from: range.start,
        to: range.end,
        client,
        comp_ids,
        venue: query.venue,
        symbol: query.symbol,
        status: query.status,
        working: query.working,
        after,
        limit: Some(limit),
    };
    let orders = store.orders(&stored).await.map_err(database_failed)?;
    let next = if orders.len() == limit { orders.last().map(|order| Next::Cursor(encode_after(order))) } else { None };
    Ok(Json(OrdersPage { orders, next }))
}

/// A database page's `next`: its last order's `<created>/<id>` (RFC 3339), in base64url: opaque,
/// so callers pass it back rather than build one.
fn encode_after(order: &OrderView) -> String {
    let text = format!("{}/{}", order.created.to_rfc3339_opts(SecondsFormat::Nanos, true), order.id.0);
    Base64UrlUnpadded::encode_string(text.as_bytes())
}

fn decode_after(text: &str) -> Result<(DateTime<Utc>, OrderId), ApiError> {
    let forged = || bad("after isn't a cursor from a range's next");
    let bytes = Base64UrlUnpadded::decode_vec(text).map_err(|_| forged())?;
    let text = String::from_utf8(bytes).map_err(|_| forged())?;
    let (created, id) = text.split_once('/').ok_or_else(forged)?;
    let created = DateTime::parse_from_rfc3339(created).map_err(|_| forged())?.to_utc();
    Ok((created, OrderId(id.parse().map_err(|_| forged())?)))
}

/// A database read's failure, logged: 500.
fn database_failed(e: std::io::Error) -> ApiError {
    error!("an API read of the database failed: {e}");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cannot read the database")
}

/// `GET /api/orders/{order_id}`: the order whose OrderID(37) is `order_id`, with every request sent
/// for it; 404 if neither the book nor the database holds it, 403 if it's not one the caller may see.
pub(super) async fn order(
    axum::extract::State(state): AxumState,
    caller: Caller,
    order_id: Result<Path<String>, PathRejection>,
) -> Result<Json<OrderDetail>, ApiError> {
    let Path(order_id) = order_id.map_err(|e| bad(e.body_text()))?;
    find_detail_for(&state, &caller, &order_id).await.map(Json)
}

/// [`find_detail`] for `caller`: 403 if the order's client is outside its scope.
pub(super) async fn find_detail_for(state: &State, caller: &Caller, order_id: &str) -> Result<OrderDetail, ApiError> {
    let detail = find_detail(state, order_id).await?;
    if caller.may_act_on(&detail.order.client) { Ok(detail) } else { Err(unseen(caller, &format!("order {order_id}"))) }
}

/// The order whose OrderID(37) is `order_id`, the book's or else the database's; 404 if neither
/// holds it. The book drops a done order at its client's day roll whether or not the writer has
/// committed it yet, so until it has, such an order reads as missing, or as its last committed
/// state.
async fn find_detail(state: &State, order_id: &str) -> Result<OrderDetail, ApiError> {
    let store = {
        let service = state.service()?;
        let gateway = service.gateway();
        if let Some(detail) = gateway.order_id(order_id).and_then(|id| gateway.order(id)) {
            return Ok(detail);
        }
        service.store().cloned()
    };
    let stored = match store {
        Some(store) => store.order(order_id).await.map_err(database_failed)?,
        None => None,
    };
    stored.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no order {order_id}")))
}

/// `GET /api/risk`: the limits in force, each session's notionals, the reference prices and the
/// blocks. A scoped caller's clients' only: see [`RiskView::retain_for`].
pub(super) async fn risk(axum::extract::State(state): AxumState, caller: Caller) -> Result<Json<RiskView>, ApiError> {
    let mut risk = state.service()?.gateway().risk();
    if let Some(comp_ids) = caller.view_scope() {
        risk.retain_for(comp_ids);
    }
    Ok(Json(risk))
}

/// `GET /api/risk/limits`: the limits in force as the configuration writes them, `{defaults,
/// clients: {COMPID: ...}}`, each table with only the keys it sets. A client's keys replace the
/// defaults'; `GET /api/risk` shows the limits merged. A scoped caller's clients' only, and the
/// defaults.
pub(super) async fn risk_limits(
    axum::extract::State(state): AxumState,
    caller: Caller,
) -> Result<Json<RiskTables>, ApiError> {
    let mut tables = state.service()?.risk_tables();
    if let Some(comp_ids) = caller.view_scope() {
        tables.client.retain(|comp_id, _| comp_ids.contains(comp_id));
    }
    Ok(Json(tables))
}

/// A routing rule as the configuration writes it (`[[route]]`): one of `venue` and `reject` set.
#[derive(Debug, Serialize)]
pub(super) struct RouteView {
    client: Option<CompactString>,
    ex_destination: Option<CompactString>,
    symbol: Option<CompactString>,
    venue: Option<CompactString>,
    reject: Option<CompactString>,
}

impl From<&Rule> for RouteView {
    fn from(rule: &Rule) -> Self {
        let (venue, reject) = match &rule.target {
            Target::Venue(venue) => (Some(venue.clone()), None),
            Target::Reject(text) => (None, Some(text.clone())),
        };
        Self {
            client: rule.client.clone(),
            ex_destination: rule.ex_destination.clone(),
            symbol: rule.symbol.clone(),
            venue,
            reject,
        }
    }
}

/// `GET /api/routes`: the routing rules in force, in the order they're tried.
pub(super) async fn routes(axum::extract::State(state): AxumState) -> Result<Json<Vec<RouteView>>, ApiError> {
    let router = state.service()?.gateway().router();
    Ok(Json(router.rules().iter().map(RouteView::from).collect()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouteTestQuery {
    /// A CompID, as rules match it.
    client: CompactString,
    ex_destination: Option<CompactString>,
    symbol: CompactString,
}

#[derive(Debug, Serialize)]
pub(super) struct RouteTest {
    /// The matching rule's index in `GET /api/routes`.
    rule: Option<usize>,
    route: Option<RouteView>,
    /// `None` is the book's "no route" rejection.
    target: Option<Target>,
}

/// `GET /api/routes/test?client=&ex_destination=&symbol=`: where the router would send such an
/// order, and by which rule. Routing only: blocks, ClOrdID limits and risk aren't checked. 403 for
/// a `client` a scoped caller may not see.
pub(super) async fn route_test(
    axum::extract::State(state): AxumState,
    caller: Caller,
    query: Result<Query<RouteTestQuery>, QueryRejection>,
) -> Result<Json<RouteTest>, ApiError> {
    let Query(query) = query.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    if !caller.may_act_on(query.client.as_str()) {
        return Err(unseen(&caller, &query.client));
    }
    let router = state.service()?.gateway().router();
    let matched = router.route_rule(&query.client, query.ex_destination.as_deref(), &query.symbol);
    Ok(Json(RouteTest {
        rule: matched.map(|(index, _)| index),
        route: matched.map(|(_, rule)| RouteView::from(rule)),
        target: matched.map(|(_, rule)| rule.target.clone()),
    }))
}

#[cfg(test)]
pub(super) mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::extract::connect_info::MockConnectInfo;
    use turbojet::{Application, Initiator, InitiatorConfig, MemoryStorage, ReconnectPolicy, SessionConfig};

    use super::super::auth::Auth;
    use super::super::tests::{Answer, send};
    use super::*;
    use crate::auth::{Role, Users};
    use crate::book::{ClientId, Scope};

    struct Nothing;

    impl Application for Nothing {}

    /// A gateway with client FUNDA logged on, venue NASDAQ down, and order 1 resting for it: the
    /// API's routes reaching it, bearer headers for each role, and what keeps them alive.
    pub(in crate::api) struct Fixture {
        pub(in crate::api) app: axum::Router,
        /// Bearer headers' values: a viewer's, a viewer's scoped to FUNDA, an operator's, an
        /// operator's scoped to FUNDB, an admin's, an admin's scoped to FUNDB, and a viewer's scoped
        /// to NASDAQ, the venue's CompID.
        pub(in crate::api) viewer: String,
        pub(in crate::api) scoped_viewer: String,
        pub(in crate::api) operator: String,
        pub(in crate::api) scoped: String,
        pub(in crate::api) admin: String,
        pub(in crate::api) scoped_admin: String,
        pub(in crate::api) venue_viewer: String,
        pub(in crate::api) service: Arc<Service>,
        pub(in crate::api) state: State,
        _client: Initiator,
        /// The configuration's, then the audit log's.
        pub(in crate::api) dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    pub(in crate::api) async fn fixture() -> Fixture {
        fixture_with("").await
    }

    /// [`fixture`] with `extra` at the end of its `jetbridge.toml`.
    pub(in crate::api) async fn fixture_with(extra: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        // Durable, as `[database]` needs.
        let sessions = "[store.disk]\nkind = \"disk\"\ndir = \"store\"\n\
                        [acceptor]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"JETBRIDGE\"\n\
                        listen = \"127.0.0.1:0\"\n[counterparty.FUNDA]\nstore = \"disk\"\n[initiator.NASDAQ]\n\
                        target_comp_id = \"NASDAQ\"\nconnect = [\"127.0.0.1:1\"]\nstore = \"disk\"\n";
        std::fs::write(dir.path().join("sessions.toml"), sessions).unwrap();
        let config = format!("sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\n{extra}");
        std::fs::write(dir.path().join("jetbridge.toml"), config).unwrap();
        let (service, _serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();

        let mut config = InitiatorConfig::new(SessionConfig::new("FIX.4.4", "FUNDA"), "JETBRIDGE");
        config.reconnect = ReconnectPolicy::fixed(Duration::from_millis(50));
        let client =
            Initiator::new(service.local_addr().to_string(), config, Arc::new(MemoryStorage::new()), Arc::new(Nothing))
                .unwrap();
        tokio::spawn(client.clone().run());
        let logged_on = || {
            let statuses = service.client_statuses();
            statuses.iter().any(|s| s.activity.as_ref().is_some_and(|a| a.state == turbojet::SessionState::LoggedOn))
        };
        for _ in 0..500 {
            if logged_on() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(logged_on(), "FUNDA didn't log on");
        crate::edge::tests::book_order(service.gateway());

        let mut users = Users::default();
        let mut bearer = |name: &str, role: Role, clients: &[&str]| {
            users.add(name, role, clients.iter().map(ToString::to_string).collect(), "a long password").unwrap();
            format!("Bearer {}", users.add_token(name).unwrap())
        };
        let viewer = bearer("vic", Role::Viewer, &[]);
        let scoped_viewer = bearer("val", Role::Viewer, &["FUNDA"]);
        let operator = bearer("olga", Role::Operator, &[]);
        let scoped = bearer("sam", Role::Operator, &["FUNDB"]);
        let admin = bearer("ada", Role::Admin, &[]);
        let scoped_admin = bearer("abe", Role::Admin, &["FUNDB"]);
        let venue_viewer = bearer("ned", Role::Viewer, &["NASDAQ"]);
        let (mut state, audit_dir) =
            super::super::tests::state(Arc::new(Auth::new(Default::default(), users, false, false)));
        state.service = Arc::downgrade(&service);
        let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let app = super::super::router(state.clone()).layer(MockConnectInfo(peer));
        Fixture {
            app,
            viewer,
            scoped_viewer,
            operator,
            scoped,
            admin,
            scoped_admin,
            venue_viewer,
            service,
            state,
            _client: client,
            dirs: (dir, audit_dir),
        }
    }

    impl Fixture {
        pub(in crate::api) async fn get(&self, path: &str) -> Answer {
            self.get_as(&self.viewer, path).await
        }

        /// `GET path` as `bearer`.
        pub(in crate::api) async fn get_as(&self, bearer: &str, path: &str) -> Answer {
            send(&self.app, "GET", path, &[("authorization", bearer)], "").await
        }

        /// `GET path`, which must answer 200: its body.
        pub(in crate::api) async fn ok(&self, path: &str) -> serde_json::Value {
            let answer = self.get(path).await;
            assert_eq!(answer.status, StatusCode::OK, "{path}: {:?}", answer.body);
            answer.body
        }
    }

    /// The client's session, logged on, then the venue's, down, by name.
    #[tokio::test]
    async fn sessions_list_both_sides() {
        let fixture = fixture().await;
        let sessions = fixture.ok("/api/sessions").await;
        let [client, venue] = sessions.as_array().unwrap().as_slice() else { panic!("{sessions}") };
        assert_eq!(client["side"], "client");
        assert_eq!(client["id"], "FIX.4.4:JETBRIDGE->FUNDA");
        assert_eq!(client["venue"], serde_json::Value::Null);
        assert_eq!(client["status"], "logged_on");
        assert_eq!(client["next_incoming"], 2);
        assert_eq!(client["paused"], false);
        assert!(client["activity"]["since"].is_string(), "{client}");
        assert_eq!(venue["side"], "venue");
        assert_eq!(venue["venue"], "NASDAQ");
        assert_eq!(venue["status"], "disconnected");
        assert_eq!(venue["next_incoming"], serde_json::Value::Null);
        assert_eq!(venue["activity"], serde_json::Value::Null);
    }

    /// The resting order, found by each filter and missed by others, paged by its numeric id.
    #[tokio::test]
    async fn orders_filter_and_page() {
        let fixture = fixture().await;
        let page = fixture.ok("/api/orders").await;
        let [order] = page["orders"].as_array().unwrap().as_slice() else { panic!("{page}") };
        let id = order["id"].as_u64().unwrap();
        assert_eq!(order["client"], "FIX.4.4:JETBRIDGE->FUNDA");
        assert_eq!(page["next"], serde_json::Value::Null);

        let found = "?client=comp:FUNDA&venue=NASDAQ&symbol=AAPL&status=pending_new&working=true";
        assert_eq!(fixture.ok(&format!("/api/orders{found}")).await["orders"].as_array().unwrap().len(), 1);
        for missed in ["?client=comp:FUNDB", "?client=session:nobody", "?symbol=MSFT", "?status=filled"] {
            assert_eq!(fixture.ok(&format!("/api/orders{missed}")).await["orders"], serde_json::json!([]), "{missed}");
        }
        let page = fixture.ok("/api/orders?limit=1").await;
        assert_eq!(page["next"], id);
        let page = fixture.ok(&format!("/api/orders?limit=1&after={id}")).await;
        assert_eq!(page, serde_json::json!({ "orders": [], "next": null }));

        let over = MAX_ORDERS_PER_PAGE + 1;
        for bad in ["limit=0", &format!("limit={over}"), "status=bogus", "client=FUNDA", "after=JB1", "colour=red"] {
            assert_eq!(fixture.get(&format!("/api/orders?{bad}")).await.status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    /// An order by its OrderID(37), with the request sent for it; an unknown one is 404.
    #[tokio::test]
    async fn an_order_by_its_order_id() {
        let fixture = fixture().await;
        let page = fixture.ok("/api/orders").await;
        let name = page["orders"][0]["order_id"].as_str().unwrap().to_string();
        let detail = fixture.ok(&format!("/api/orders/{name}")).await;
        assert_eq!(detail["order_id"], name.as_str());
        assert_eq!(detail["requests"][0]["kind"], "new");
        assert_eq!(fixture.get("/api/orders/NOPE1").await.status, StatusCode::NOT_FOUND);
        // Not UTF-8: a JSON 400.
        let answer = fixture.get("/api/orders/%FF").await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert!(answer.body["error"].is_string(), "{}", answer.body);
    }

    /// A fixture with an in-memory database holding orders 100 to 102 of FUNDA, done, created
    /// 1, 2 and 3 October 2026 at 09:00 UTC: the book doesn't hold them.
    pub(in crate::api) async fn with_history(extra: &str) -> Fixture {
        let fixture = fixture_with(&format!("[database]\nurl = \"sqlite::memory:\"\n{extra}")).await;
        let client = ClientId::from("FUNDA");
        let written: Vec<_> =
            (100..=102).map(|id| crate::store::tests::dated(id, &client, None, true, past(id - 99))).collect();
        fixture.service.store().unwrap().write(&written).await.unwrap();
        fixture
    }

    /// 09:00 UTC on `day` October 2026.
    pub(in crate::api) fn past(day: u64) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, day.try_into().unwrap(), 9, 0, 0).unwrap()
    }

    /// The OrderIDs of a page, and its `next`.
    fn names(page: &serde_json::Value) -> (Vec<&str>, &serde_json::Value) {
        (
            page["orders"].as_array().unwrap().iter().map(|order| order["order_id"].as_str().unwrap()).collect(),
            &page["next"],
        )
    }

    /// With a range, the orders are the database's, those the book has dropped too, by creation;
    /// without, the book's alone. A session not connected is still found, and a past order's
    /// detail too.
    #[tokio::test]
    async fn a_range_reads_past_orders_from_the_database() {
        let fixture = with_history("").await;
        let range = "from=2026-10-01T00:00:00Z&to=2026-10-04T00:00:00Z";
        let page = fixture.ok(&format!("/api/orders?{range}")).await;
        assert_eq!(names(&page), (vec!["A100", "A101", "A102"], &serde_json::Value::Null));
        assert_eq!(page["orders"][0]["created"], "2026-10-01T09:00:00Z");
        // `from` is included, `to` isn't.
        let page = fixture.ok("/api/orders?from=2026-10-01T09:00:00Z&to=2026-10-02T09:00:00Z").await;
        assert_eq!(names(&page).0, ["A100"]);
        let page = fixture.ok("/api/orders?from=2026-10-02T10:00:00Z&to=2026-10-04T00:00:00Z").await;
        assert_eq!(names(&page).0, ["A102"]);
        assert!(!names(&fixture.ok("/api/orders").await).0.contains(&"A100"));

        let found = |query: &'static str| {
            let fixture = &fixture;
            async move { names(&fixture.ok(&format!("/api/orders?{range}&{query}")).await).0.len() }
        };
        assert_eq!(found("client=session:FIX.4.4:JETBRIDGE-%3EFUNDA&status=filled&venue=NASDAQ").await, 3);
        assert_eq!(found("client=comp:FUNDA&symbol=AAPL").await, 3);
        for missed in ["client=comp:FUNDB", "client=session:FIX.4.2:JETBRIDGE-%3EFUNDA", "working=true", "symbol=MSFT"]
        {
            assert_eq!(found(missed).await, 0, "{missed}");
        }

        let detail = fixture.ok("/api/orders/A101").await;
        assert_eq!(detail["order_id"], "A101");
        assert_eq!(detail["requests"][0]["kind"], "new");
        assert_eq!(fixture.get("/api/orders/A999").await.status, StatusCode::NOT_FOUND);
    }

    /// A range pages by its opaque `next`, to a page that isn't full.
    #[tokio::test]
    async fn a_range_pages_by_its_cursor() {
        let fixture = with_history("").await;
        let range = "from=2026-10-01T00:00:00Z&to=2026-10-04T00:00:00Z&limit=1";
        let mut seen = Vec::new();
        let mut after = String::new();
        loop {
            let page = fixture.ok(&format!("/api/orders?{range}{after}")).await;
            let (page_names, next) = names(&page);
            seen.extend(page_names.iter().map(ToString::to_string));
            let Some(next) = next.as_str() else { break };
            after = format!("&after={next}");
        }
        assert_eq!(seen, ["A100", "A101", "A102"]);
    }

    /// A range too long, empty or backwards, `to` alone, a time that isn't one, an `after` that
    /// isn't a range's, or a range without a database: 400, each for its own reason.
    #[tokio::test]
    async fn bad_ranges_are_refused() {
        let fixture = with_history("").await;
        let range = "from=2026-10-01T00:00:00Z&to=2026-10-04T00:00:00Z";
        let id = fixture.ok("/api/orders").await["orders"][0]["id"].as_u64().unwrap();
        let from = "from=2026-10-01T00:00:00Z";
        let cases = [
            (format!("{from}&to=2026-11-01T00:00:01Z"), "a range covers 31 days at most"),
            (format!("{from}&to=2026-10-01T00:00:00Z"), "from must be before to"),
            (format!("{from}&to=2026-09-30T00:00:00Z"), "from must be before to"),
            ("to=2026-10-04T00:00:00Z".to_string(), "to needs from"),
            (format!("{range}&after=JB1"), "after isn't a cursor from a range's next"),
            (format!("{range}&after={id}"), "after isn't a cursor from a range's next"),
        ];
        for (query, error) in cases {
            let answer = fixture.get(&format!("/api/orders?{query}")).await;
            assert_eq!(
                (answer.status, answer.body["error"].as_str()),
                (StatusCode::BAD_REQUEST, Some(error)),
                "{query}"
            );
        }
        let answer = fixture.get("/api/orders?from=yesterday&to=2026-10-04T00:00:00Z").await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert!(answer.body["error"].as_str().unwrap().contains("from"), "{}", answer.body);
        // 31 days exactly is taken.
        fixture.ok(&format!("/api/orders?{from}&to=2026-11-01T00:00:00Z")).await;

        let without = fixture_with("").await;
        for query in [range.to_string(), format!("{range}&after=JB1")] {
            let answer = without.get(&format!("/api/orders?{query}")).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST);
            assert_eq!(answer.body["error"], "no [database]: only the current day is kept", "{query}");
        }
    }

    /// `to` is now by default: a range from an hour ago is taken, and holds none of the past
    /// orders; one from an hour ahead is empty, so refused.
    #[tokio::test]
    async fn to_is_now_by_default() {
        let fixture = with_history("").await;
        let from = |hours| (Utc::now() + TimeDelta::hours(hours)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let page = fixture.ok(&format!("/api/orders?from={}", from(-1))).await;
        assert!(!names(&page).0.iter().any(|name| name.starts_with("A10")), "{page}");
        let answer = fixture.get(&format!("/api/orders?from={}", from(1))).await;
        assert_eq!(
            (answer.status, answer.body["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("from must be before to"))
        );
    }

    /// A cursor is a position, not a range: replayed against a range wholly before it, it finds
    /// nothing.
    #[tokio::test]
    async fn a_cursor_replayed_against_an_earlier_range_finds_nothing() {
        let fixture = with_history("").await;
        let page = fixture.ok("/api/orders?from=2026-10-02T00:00:00Z&to=2026-10-04T00:00:00Z&limit=1").await;
        let next = page["next"].as_str().unwrap();
        let page =
            fixture.ok(&format!("/api/orders?from=2026-10-01T00:00:00Z&to=2026-10-02T00:00:00Z&after={next}")).await;
        assert_eq!(page, serde_json::json!({ "orders": [], "next": null }));
    }

    /// A session scope matches the whole of the session's name, a SubID's `/` and all: FUNDA's
    /// desk is another session than FUNDA, of the same CompID.
    #[tokio::test]
    async fn a_session_scope_matches_the_whole_name() {
        let fixture = with_history("").await;
        let desk = ClientId::new(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA").with_target_sub_id("DESK"));
        assert_eq!(desk.session(), "FIX.4.4:JETBRIDGE->FUNDA/DESK");
        let written = crate::store::tests::dated(103, &desk, None, true, past(3) + TimeDelta::hours(1));
        fixture.service.store().unwrap().write(&[written]).await.unwrap();
        let range = "from=2026-10-01T00:00:00Z&to=2026-10-04T00:00:00Z";
        let found = async |client: &str| {
            let page = fixture.ok(&format!("/api/orders?{range}&client={client}")).await;
            names(&page).0.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        assert_eq!(found("session:FIX.4.4:JETBRIDGE-%3EFUNDA%2FDESK").await, ["A103"]);
        assert_eq!(found("session:FIX.4.4:JETBRIDGE-%3EFUNDA").await, ["A100", "A101", "A102"]);
        assert_eq!(found("session:FIX.4.4:JETBRIDGE-%3EFUNDA%2FDES").await, Vec::<String>::new());
        assert_eq!(found("comp:FUNDA").await, ["A100", "A101", "A102", "A103"]);
    }

    /// A database read that fails is a 500, for a range and for an order the book doesn't hold.
    #[tokio::test]
    async fn a_failed_database_read_is_500() {
        let fixture = with_history("").await;
        fixture.service.store().unwrap().close_reads().await;
        let answer = fixture.get("/api/orders?from=2026-10-01T00:00:00Z&to=2026-10-04T00:00:00Z").await;
        assert_eq!(
            (answer.status, answer.body["error"].as_str()),
            (StatusCode::INTERNAL_SERVER_ERROR, Some("cannot read the database"))
        );
        assert_eq!(fixture.get("/api/orders/A100").await.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Risk counts the resting order's session; the routes are the file's.
    #[tokio::test]
    async fn risk_and_routes() {
        let fixture = fixture().await;
        let risk = fixture.ok("/api/risk").await;
        assert_eq!(risk["sessions"][0]["client"], "FIX.4.4:JETBRIDGE->FUNDA");
        assert_eq!(risk["blocked"], serde_json::json!([]));
        let routes = fixture.ok("/api/routes").await;
        let route = serde_json::json!({
            "client": null, "ex_destination": null, "symbol": null, "venue": "NASDAQ", "reject": null
        });
        assert_eq!(routes, serde_json::json!([route]));
    }

    /// The defaults and each client's own keys as the file writes them: a key left out isn't
    /// shown, and an amount keeps its form (integer, float or string).
    #[tokio::test]
    async fn risk_limits_as_written() {
        let fixture = fixture().await;
        assert_eq!(fixture.ok("/api/risk/limits").await, serde_json::json!({ "defaults": {}, "clients": {} }));
        let config = "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\n\
                      [risk.defaults]\nmax_order_qty = 1000\nprice_band = 5.5\nsymbols = [\"AAPL\"]\n\
                      [risk.client.FUNDA]\nmax_notional = \"250000.50\"\nself_trade = true\n";
        std::fs::write(fixture.dirs.0.path().join("jetbridge.toml"), config).unwrap();
        let service = fixture.service.clone();
        let reloaded = tokio::task::spawn_blocking(move || service.reload()).await.unwrap().unwrap();
        assert!(reloaded.config.is_ok(), "{:?}", reloaded.config);
        let expected = serde_json::json!({
            "defaults": { "max_order_qty": 1000, "price_band": 5.5, "symbols": ["AAPL"] },
            "clients": { "FUNDA": { "max_notional": "250000.50", "self_trade": true } },
        });
        assert_eq!(fixture.ok("/api/risk/limits").await, expected);
    }

    /// A dry run names the first rule matching by client, ExDestination or symbol, and its target;
    /// none matching is null throughout, and `client` and `symbol` are required. A scoped caller
    /// tests only its clients' orders.
    #[tokio::test]
    async fn a_route_test_names_the_first_matching_rule() {
        let fixture = fixture().await;
        let config = "sessions = \"sessions.toml\"\n[[route]]\nsymbol = \"MSFT\"\nreject = \"not MSFT\"\n\
                      [[route]]\nex_destination = \"XNAS\"\nvenue = \"NASDAQ\"\n\
                      [[route]]\nclient = \"FUNDA\"\nvenue = \"NASDAQ\"\n";
        std::fs::write(fixture.dirs.0.path().join("jetbridge.toml"), config).unwrap();
        let service = fixture.service.clone();
        let reloaded = tokio::task::spawn_blocking(move || service.reload()).await.unwrap().unwrap();
        assert!(reloaded.config.is_ok(), "{:?}", reloaded.config);

        let test = async |query: &str| fixture.ok(&format!("/api/routes/test?{query}")).await;
        // MSFT matches rules 0 and 2: the first wins.
        let answer = test("client=FUNDA&symbol=MSFT").await;
        assert_eq!(answer["rule"], 0);
        assert_eq!(answer["route"]["symbol"], "MSFT");
        assert_eq!(answer["target"], serde_json::json!({ "reject": "not MSFT" }));
        let answer = test("client=FUNDB&ex_destination=XNAS&symbol=AAPL").await;
        assert_eq!(answer["rule"], 1);
        assert_eq!(answer["target"], serde_json::json!({ "venue": "NASDAQ" }));
        assert_eq!(test("client=FUNDA&symbol=AAPL").await["rule"], 2);
        let none = serde_json::json!({ "rule": null, "route": null, "target": null });
        assert_eq!(test("client=FUNDB&symbol=AAPL").await, none);

        for bad in ["symbol=AAPL", "client=FUNDA", "client=FUNDA&symbol=AAPL&colour=red"] {
            assert_eq!(fixture.get(&format!("/api/routes/test?{bad}")).await.status, StatusCode::BAD_REQUEST, "{bad}");
        }

        // FUNDA's viewer tests FUNDA's orders alone, and sees every rule.
        let as_funda = async |path: &str| fixture.get_as(&fixture.scoped_viewer, path).await;
        assert_eq!(as_funda("/api/routes/test?client=FUNDA&symbol=AAPL").await.body["rule"], 2);
        let answer = as_funda("/api/routes/test?client=FUNDB&symbol=AAPL").await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(answer.body["error"], "val may see only clients FUNDA, not FUNDB");
        assert_eq!(as_funda("/api/routes").await.body.as_array().unwrap().len(), 3);
    }

    /// The resting FUNDA order, read as FUNDA's viewer, FUNDB's operator and unscoped: a scoped
    /// caller's page holds its clients' orders alone, a `client` naming another's (a CompID, a
    /// session known or not, or all) is 403, and so is another's order by its OrderID.
    #[tokio::test]
    async fn a_scoped_caller_reads_its_clients_orders() {
        let fixture = fixture().await;
        let (funda, fundb) = (fixture.scoped_viewer.as_str(), fixture.scoped.as_str());
        let count = async |bearer: &str, query: &str| {
            let answer = fixture.get_as(bearer, &format!("/api/orders{query}")).await;
            assert_eq!(answer.status, StatusCode::OK, "{query}: {}", answer.body);
            answer.body["orders"].as_array().unwrap().len()
        };
        let session = |target: &str| format!("?client=session:FIX.4.4:JETBRIDGE-%3E{target}");
        assert_eq!(count(funda, "").await, 1);
        assert_eq!(count(funda, "?client=comp:FUNDA").await, 1);
        assert_eq!(count(funda, &session("FUNDA")).await, 1);
        // A session of its CompID the gateway hasn't seen: allowed, and empty.
        assert_eq!(count(funda, &session("FUNDA%2FDESK")).await, 0);
        assert_eq!(count(fundb, "").await, 0);
        assert_eq!(count(fundb, "?client=comp:FUNDB").await, 0);
        assert_eq!(count(fixture.viewer.as_str(), "").await, 1);
        assert_eq!(count(fixture.viewer.as_str(), "?client=comp:FUNDB").await, 0);
        let refused = [
            (funda, "?client=comp:FUNDB".to_string()),
            (funda, "?client=all".to_string()),
            (funda, session("FUNDB")),
            (funda, session("FUNDAX")),
            (fundb, "?client=comp:FUNDA".to_string()),
            (fundb, session("FUNDA")),
        ];
        for (bearer, query) in refused {
            let answer = fixture.get_as(bearer, &format!("/api/orders{query}")).await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{query}: {}", answer.body);
        }
        let answer = fixture.get_as(funda, "/api/orders?client=comp:FUNDB").await;
        assert_eq!(answer.body["error"], "val may see only clients FUNDA, not comp:FUNDB");

        let name = fixture.ok("/api/orders").await["orders"][0]["order_id"].as_str().unwrap().to_string();
        assert_eq!(fixture.get_as(funda, &format!("/api/orders/{name}")).await.status, StatusCode::OK);
        let answer = fixture.get_as(fundb, &format!("/api/orders/{name}")).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(answer.body["error"], format!("sam may see only clients FUNDB, not order {name}"));
    }

    /// As [`a_scoped_caller_reads_its_clients_orders`], from the database, with a past order of
    /// FUNDB's desk the gateway has never seen: a session it doesn't know is checked by its name.
    #[tokio::test]
    async fn a_scoped_caller_reads_its_clients_past_orders() {
        let fixture = with_history("").await;
        let desk = ClientId::new(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDB").with_target_sub_id("DESK"));
        let written = crate::store::tests::dated(103, &desk, None, true, past(3) + TimeDelta::hours(1));
        fixture.service.store().unwrap().write(&[written]).await.unwrap();
        let (funda, fundb) = (fixture.scoped_viewer.as_str(), fixture.scoped.as_str());
        let range = "from=2026-10-01T00:00:00Z&to=2026-10-04T00:00:00Z";
        let found = async |bearer: &str, query: &str| {
            let answer = fixture.get_as(bearer, &format!("/api/orders?{range}{query}")).await;
            assert_eq!(answer.status, StatusCode::OK, "{query}: {}", answer.body);
            names(&answer.body).0.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        let session = |target: &str| format!("&client=session:FIX.4.4:JETBRIDGE-%3E{target}");
        assert_eq!(found(funda, "").await, ["A100", "A101", "A102"]);
        assert_eq!(found(funda, &session("FUNDA")).await, ["A100", "A101", "A102"]);
        assert_eq!(found(funda, "&client=session:FIX.4.2:JETBRIDGE-%3EFUNDA").await, Vec::<String>::new());
        assert_eq!(found(fundb, "").await, ["A103"]);
        assert_eq!(found(fundb, "&client=comp:FUNDB").await, ["A103"]);
        assert_eq!(found(fundb, &session("FUNDB%2FDESK")).await, ["A103"]);
        assert_eq!(found(fixture.viewer.as_str(), "").await, ["A100", "A101", "A102", "A103"]);
        let refused = [
            (funda, "&client=comp:FUNDB".to_string()),
            (funda, "&client=all".to_string()),
            (funda, session("FUNDB%2FDESK")),
            (fundb, session("FUNDA")),
            (fundb, "&client=session:FIX.4.2:JETBRIDGE-%3EFUNDA".to_string()),
        ];
        for (bearer, query) in refused {
            let answer = fixture.get_as(bearer, &format!("/api/orders?{range}{query}")).await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{query}: {}", answer.body);
        }

        let status =
            async |bearer: &str, name: &str| fixture.get_as(bearer, &format!("/api/orders/{name}")).await.status;
        assert_eq!(status(funda, "A100").await, StatusCode::OK);
        assert_eq!(status(fundb, "A100").await, StatusCode::FORBIDDEN);
        assert_eq!(status(fundb, "A103").await, StatusCode::OK);
        assert_eq!(status(funda, "A103").await, StatusCode::FORBIDDEN);
    }

    /// The target of a session's name: its TargetCompID, whole, before any SubID, LocationID or
    /// qualifier.
    #[test]
    fn a_sessions_name_names_its_target() {
        assert!(names_target("FIX.4.4:JETBRIDGE->FUNDA", "FUNDA"));
        assert!(names_target("FIX.4.4:JETBRIDGE/S->FUNDA/DESK/NY", "FUNDA"));
        assert!(names_target("FIX.4.4:JETBRIDGE->FUNDA:Q", "FUNDA"));
        assert!(!names_target("FIX.4.4:JETBRIDGE->FUNDAX", "FUNDA"));
        assert!(!names_target("FIX.4.4:FUNDA->JETBRIDGE", "FUNDA"));
        assert!(!names_target("FUNDA", "FUNDA"));
    }

    /// Sessions, risk and limits as FUNDA's viewer, FUNDB's operator and unscoped: a scoped
    /// caller's clients' sessions (FUNDB's paused one too), notionals, limits and blocks (of a
    /// CompID or a session) alone, `all` covering them too, while the venues, the defaults and the
    /// reference prices are everyone's.
    #[tokio::test]
    async fn a_scoped_caller_reads_its_clients_sessions_and_risk() {
        let fixture = fixture().await;
        let config = "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\n\
                      [risk.defaults]\nmax_order_qty = 1000\n\
                      [risk.client.FUNDA]\nmax_notional = 1\n[risk.client.FUNDB]\nmax_notional = 2\n";
        std::fs::write(fixture.dirs.0.path().join("jetbridge.toml"), config).unwrap();
        let service = fixture.service.clone();
        let reloaded = tokio::task::spawn_blocking(move || service.reload()).await.unwrap().unwrap();
        assert!(reloaded.config.is_ok(), "{:?}", reloaded.config);
        let gateway = fixture.service.gateway();
        let funda = ClientId::new(&fixture.service.client_statuses()[0].id);
        let blocks = [
            Scope::CompId("FUNDA".into()),
            Scope::CompId("FUNDB".into()),
            Scope::All,
            Scope::Session(funda),
            Scope::Session(ClientId::from("FUNDB")),
        ];
        for scope in blocks {
            assert!(gateway.block(scope).await);
        }
        fixture.service.client_session(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDB")).pause(None);
        let read = async |bearer: &str, path: &str| {
            let answer = fixture.get_as(bearer, path).await;
            assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.body);
            answer.body
        };
        let keys = |value: &serde_json::Value| value.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        let funda_session = "FIX.4.4:JETBRIDGE->FUNDA";
        let cases = [
            (fixture.scoped_viewer.as_str(), vec![funda_session], vec!["FUNDA"]),
            (fixture.scoped.as_str(), vec![], vec!["FUNDB"]),
            (fixture.viewer.as_str(), vec![funda_session], vec!["FUNDA", "FUNDB"]),
        ];
        for (bearer, risk_sessions, comp_ids) in cases {
            let sessions = read(bearer, "/api/sessions").await;
            let shown: Vec<_> = sessions.as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
            let expected: Vec<_> = comp_ids.iter().map(|c| format!("FIX.4.4:JETBRIDGE->{c}")).collect();
            assert_eq!(shown, [expected, vec!["FIX.4.4:JETBRIDGE->NASDAQ".to_string()]].concat());
            let risk = read(bearer, "/api/risk").await;
            let shown: Vec<_> =
                risk["sessions"].as_array().unwrap().iter().map(|s| s["client"].as_str().unwrap()).collect();
            assert_eq!(shown, risk_sessions);
            assert_eq!(keys(&risk["by_comp_id"]), comp_ids);
            let blocked = [
                "comp:FUNDA",
                "comp:FUNDB",
                "all",
                "session:FIX.4.4:JETBRIDGE->FUNDA",
                "session:FIX.4.4:JETBRIDGE->FUNDB",
            ];
            let blocked: Vec<_> = blocked
                .into_iter()
                .filter(|name| name == &"all" || comp_ids.iter().any(|c| name.ends_with(*c)))
                .collect();
            assert_eq!(risk["blocked"], serde_json::json!(blocked));
            assert!(risk["defaults"].is_object(), "{risk}");
            assert!(risk["references"].is_object(), "{risk}");
            let limits = read(bearer, "/api/risk/limits").await;
            assert_eq!(limits["defaults"], serde_json::json!({ "max_order_qty": 1000 }));
            assert_eq!(keys(&limits["clients"]), comp_ids);
        }
    }

    /// Once the service has gone, reads answer 503.
    #[tokio::test]
    async fn reads_answer_503_once_the_service_is_gone() {
        let fixture = fixture().await;
        let Fixture { app, viewer, service, .. } = fixture;
        drop(service);
        let answer = send(&app, "GET", "/api/risk", &[("authorization", &viewer)], "").await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
