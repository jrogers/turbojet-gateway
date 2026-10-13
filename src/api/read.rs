//! The API's reads [viewer]: the sessions, the book's orders, risk and routes, as JSON. Every role
//! may read, so being behind `authenticate` is the only check; reads aren't audited.
//!
//! The orders are the book's: the working ones and each client's done orders of its current day.

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use turbojet::fields::CompactString;
use turbojet::{SessionId, SessionStatus};

use super::{ApiError, State};
use crate::book::{MAX_ORDERS_PER_PAGE, OrderDetail, OrderFilter, OrderId, OrderView, RiskView, ScopeName, Status};
use crate::config::RiskTables;
use crate::router::{Rule, Target};
use crate::service::Service;

/// Orders a page of `GET /api/orders` holds without a `limit`: a screenful.
const ORDERS_PAGE_DEFAULT: usize = 100;

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
/// connected, then every venue's, connected or not, by name.
pub(super) async fn sessions(axum::extract::State(state): AxumState) -> Result<Json<Vec<SessionView>>, ApiError> {
    Ok(Json(session_views(&*state.service()?)))
}

/// What [`sessions`] answers.
pub(super) fn session_views(service: &Service) -> Vec<SessionView> {
    let mut clients = service.client_statuses();
    clients.sort_by_cached_key(|status| status.id.to_string());
    let mut sessions: Vec<_> = clients
        .iter()
        .map(|status| SessionView::new(SessionSide::Client, &status.id, status.paused, Some(status)))
        .collect();
    // A paused client is logged out, and would otherwise vanish from the list it's resumed from.
    let mut paused = service.client_paused();
    paused.retain(|id| !clients.iter().any(|status| &status.id == id));
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
    /// The previous page's `next`.
    after: Option<u64>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(super) struct OrdersPage {
    orders: Vec<OrderView>,
    /// The `after` for the next page; `None` when this page is short. A full page may be followed
    /// by an empty one.
    next: Option<u64>,
}

/// `GET /api/orders?client=&venue=&symbol=&status=&working=&after=&limit=`: the book's orders
/// matching every filter given, by OrderId, `limit` at a time (by default [`ORDERS_PAGE_DEFAULT`],
/// at most [`MAX_ORDERS_PER_PAGE`]). `after` and `next` are the numeric `id`, not the wire
/// OrderID, so a cursor holds when its order leaves the book.
pub(super) async fn orders(
    axum::extract::State(state): AxumState,
    query: Result<Query<OrdersQuery>, QueryRejection>,
) -> Result<Json<OrdersPage>, ApiError> {
    let Query(query) = query.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let limit = query.limit.unwrap_or(ORDERS_PAGE_DEFAULT);
    if !(1..=MAX_ORDERS_PER_PAGE).contains(&limit) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, format!("limit must be 1 to {MAX_ORDERS_PER_PAGE}")));
    }
    let client = match &query.client {
        Some(text) => {
            Some(text.parse::<ScopeName>().map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?)
        }
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
        venue: query.venue,
        symbol: query.symbol,
        status: query.status,
        working: query.working,
        after: query.after.map(OrderId),
        limit: Some(limit),
    };
    let orders = gateway.orders(&filter);
    // Whether more match isn't known without a walk past the page: a full page may be the last.
    let next = if orders.len() == limit { orders.last().map(|order| order.id.0) } else { None };
    Ok(Json(OrdersPage { orders, next }))
}

/// `GET /api/orders/{order_id}`: the order whose OrderID(37) is `order_id`, with every request sent
/// for it; 404 if the book doesn't hold it.
pub(super) async fn order(
    axum::extract::State(state): AxumState,
    order_id: Result<Path<String>, PathRejection>,
) -> Result<Json<OrderDetail>, ApiError> {
    let Path(order_id) = order_id.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
    let service = state.service()?;
    let gateway = service.gateway();
    gateway
        .order_id(&order_id)
        .and_then(|id| gateway.order(id))
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no order {order_id}")))
}

/// `GET /api/risk`: the limits in force, each session's notionals, the reference prices and the
/// blocks.
pub(super) async fn risk(axum::extract::State(state): AxumState) -> Result<Json<RiskView>, ApiError> {
    Ok(Json(state.service()?.gateway().risk()))
}

/// `GET /api/risk/limits`: the limits in force as the configuration writes them, `{defaults,
/// clients: {COMPID: ...}}`, each table with only the keys it sets. A client's keys replace the
/// defaults'; `GET /api/risk` shows the limits merged.
pub(super) async fn risk_limits(axum::extract::State(state): AxumState) -> Result<Json<RiskTables>, ApiError> {
    Ok(Json(state.service()?.risk_tables()))
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
/// order, and by which rule. Routing only: blocks, ClOrdID limits and risk aren't checked.
pub(super) async fn route_test(
    axum::extract::State(state): AxumState,
    query: Result<Query<RouteTestQuery>, QueryRejection>,
) -> Result<Json<RouteTest>, ApiError> {
    let Query(query) = query.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))?;
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

    struct Nothing;

    impl Application for Nothing {}

    /// A gateway with client FUNDA logged on, venue NASDAQ down, and order 1 resting for it: the
    /// API's routes reaching it, bearer headers for each role, and what keeps them alive.
    pub(in crate::api) struct Fixture {
        pub(in crate::api) app: axum::Router,
        /// Bearer headers' values: a viewer's, an operator's, an operator's scoped to FUNDB, an
        /// admin's, and an admin's scoped to FUNDB.
        pub(in crate::api) viewer: String,
        pub(in crate::api) operator: String,
        pub(in crate::api) scoped: String,
        pub(in crate::api) admin: String,
        pub(in crate::api) scoped_admin: String,
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
        let sessions = "[acceptor]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"JETBRIDGE\"\n\
                        listen = \"127.0.0.1:0\"\n[counterparty.FUNDA]\n[initiator.NASDAQ]\n\
                        target_comp_id = \"NASDAQ\"\nconnect = [\"127.0.0.1:1\"]\n";
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
        let operator = bearer("olga", Role::Operator, &[]);
        let scoped = bearer("sam", Role::Operator, &["FUNDB"]);
        let admin = bearer("ada", Role::Admin, &[]);
        let scoped_admin = bearer("abe", Role::Admin, &["FUNDB"]);
        let (mut state, audit_dir) =
            super::super::tests::state(Arc::new(Auth::new(Default::default(), users, false, false)));
        state.service = Arc::downgrade(&service);
        let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let app = super::super::router(state.clone()).layer(MockConnectInfo(peer));
        Fixture {
            app,
            viewer,
            operator,
            scoped,
            admin,
            scoped_admin,
            service,
            state,
            _client: client,
            dirs: (dir, audit_dir),
        }
    }

    impl Fixture {
        pub(in crate::api) async fn get(&self, path: &str) -> Answer {
            send(&self.app, "GET", path, &[("authorization", &self.viewer)], "").await
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
    /// none matching is null throughout, and `client` and `symbol` are required.
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
