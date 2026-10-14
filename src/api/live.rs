//! `GET /api/live` [viewer]: a websocket of what changes, for a console. It sends a snapshot,
//! `{"type":"snapshot", sessions, orders, orders_next, blocked, unavailable}`, then each event as
//! it happens: the gateway's (`order`, `blocked`, `unblocked` and `alarm`; see [`GatewayEvent`])
//! and the client and venue sessions' (`session`). What the client sends is ignored.
//!
//! A viewer scoped to some clients (see [`Caller::view_scope`]) is sent what concerns them: their
//! orders, sessions and blocks (and blocks of all), every venue session, and the alarms
//! [`Alarm::visible_to`](crate::book::Alarm::visible_to) them. Each recheck reads the viewer's
//! clients again, and a change sends a snapshot under the new scope, so nothing outside it stays.
//!
//! A console applies the events to the snapshot in the order they come. An `order` event is the
//! order's whole view, to put in place of the one held (or add); `blocked` and `unblocked` add and
//! remove a scope; a `session` event says what happened to the session keyed by (`side`, `id`).
//!
//! The events are subscribed to before the snapshot is read, so none falls between the two; the
//! first few may repeat what the snapshot already shows. A socket that falls behind any
//! channel's capacity skips to the channels' ends and gets a new snapshot, rather than the
//! gateway waiting on it: the channels bound what a slow socket costs. One that falls behind
//! again within [`MIN_SNAPSHOT_GAP`] is closed (1013), as it can't keep up.
//!
//! The upgrade authenticates as any request does, with the cookie or a bearer token, never a token
//! in the URL, which proxies log. With the cookie its `Origin` must be the API's own host (see
//! [`auth`](super::auth)). The socket closes when the server stops, and within [`RECHECK`] of its
//! session ending or its token being revoked. Like other reads, it isn't audited.
//!
//! Each socket holds one of the API's connections for as long as it's open, so there are at most
//! [`MAX_LIVE_SOCKETS`] (503 past that) and [`MAX_LIVE_SOCKETS_PER_USER`] for a user (429).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::info;
use turbojet::{SessionEvent, SessionEventKind};

use super::read::{SessionSide, SessionView, session_views};
use super::{ApiError, Caller, State};
use crate::book::{GatewayEvent, MAX_ORDERS_PER_PAGE, OrderView, Scope, is_one_of};

/// How often an open socket checks that its session or token still holds. Checking is a map
/// lookup; a removed user or a revoked token keeps watching this long at most.
const RECHECK: Duration = Duration::from_secs(10);
// Its pings keep an idle socket's connection open.
const _: () = assert!(RECHECK.as_secs() < super::listener::IDLE.as_secs());

/// Live sockets open at once. Each holds one of the API's 256 connections for as long as it's
/// open, so this leaves most for requests, and still allows a console for every operator.
const MAX_LIVE_SOCKETS: usize = 64;

/// Live sockets a user may have open at once: a few consoles, not a script that never closes
/// its sockets taking every one.
const MAX_LIVE_SOCKETS_PER_USER: usize = 8;

const _: () = assert!(MAX_LIVE_SOCKETS_PER_USER <= MAX_LIVE_SOCKETS);

/// The least time between a socket's snapshots. Each walks the book under the gateway's lock, so a
/// client too slow to keep up mustn't cost one per few thousand events: lagging again sooner
/// closes its socket.
const MIN_SNAPSHOT_GAP: Duration = Duration::from_secs(1);

/// Pings, one each [`RECHECK`], a client may leave unanswered before its socket is closed: on a
/// quiet gateway nothing else is sent, and a peer gone without a word would otherwise hold its
/// connection until its session ends, or for ever with a token.
const MAX_SILENT_TICKS: u32 = 3;

/// The longest close reason, in bytes: a control frame carries 125, two of them the code.
const MAX_CLOSE_REASON_LEN: usize = 123;

const STOPPING: &str = "the gateway is stopping";
const TOO_SLOW: &str = "too slow to keep up: reconnect";
const SILENT: &str = "no answer to pings";

const _: () = assert!(STOPPING.len() <= MAX_CLOSE_REASON_LEN);
const _: () = assert!(TOO_SLOW.len() <= MAX_CLOSE_REASON_LEN);
const _: () = assert!(SILENT.len() <= MAX_CLOSE_REASON_LEN);

/// How long a send may wait for a client that isn't reading before its socket is dropped, so a
/// stalled client can't hold its task, and its connection, for ever.
const SEND_WAIT: Duration = Duration::from_secs(30);

/// The largest message or frame a client may send, in bytes. It has nothing to send but pings
/// and a close, so this is axum's 64 MiB default cut down to what those need.
const MAX_RECEIVED_LEN: usize = 4_096;

/// The channels a socket reads, subscribed before its first snapshot.
pub(super) struct Events {
    gateway: broadcast::Receiver<GatewayEvent>,
    clients: broadcast::Receiver<SessionEvent>,
    venues: broadcast::Receiver<SessionEvent>,
}

impl Events {
    /// Skips what each channel holds: after a lag, the snapshot shows it, and replaying a backlog
    /// of thousands of stale events would only make the socket lag again.
    fn skip_to_ends(&mut self) {
        self.gateway = self.gateway.resubscribe();
        self.clients = self.clients.resubscribe();
        self.venues = self.venues.resubscribe();
    }
}

/// The live sockets open, in all and by user.
#[derive(Debug, Default)]
pub(super) struct Sockets(Mutex<Counts>);

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    /// Users with a socket open, and how many: never a zero.
    by_user: HashMap<String, usize>,
}

/// An open socket, counted in its [`Sockets`] until dropped.
pub(super) struct Counted {
    sockets: Arc<Sockets>,
    user: String,
}

impl Sockets {
    /// Counts a socket for `user`, or refuses it: 503 if [`MAX_LIVE_SOCKETS`] are open, 429 if
    /// `user` has [`MAX_LIVE_SOCKETS_PER_USER`].
    fn open(self: &Arc<Self>, user: &str) -> Result<Counted, ApiError> {
        let mut counts = self.lock();
        let counts = &mut *counts;
        if counts.total >= MAX_LIVE_SOCKETS {
            return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "too many live sockets open: try later"));
        }
        let mine = counts.by_user.entry(user.to_string()).or_default();
        if *mine >= MAX_LIVE_SOCKETS_PER_USER {
            let message = format!("{user} has {MAX_LIVE_SOCKETS_PER_USER} live sockets open already");
            return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, message));
        }
        *mine += 1;
        counts.total += 1;
        debug_assert!(counts.by_user.len() <= counts.total);
        Ok(Counted { sockets: self.clone(), user: user.to_string() })
    }

    /// The counts. A panic while they were held can't have left them half-changed: each change is
    /// made whole before anything that could panic.
    fn lock(&self) -> MutexGuard<'_, Counts> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        let mut counts = self.sockets.lock();
        let counts = &mut *counts;
        let mine = counts.by_user.get_mut(&self.user).expect("a counted socket's user is counted");
        *mine -= 1;
        if *mine == 0 {
            counts.by_user.remove(&self.user);
        }
        counts.total -= 1;
    }
}

/// Whose socket it is, and what its rechecks authenticate.
pub(super) struct Viewer {
    user: String,
    ip: IpAddr,
    /// The CompIDs whose clients the viewer sees, as [`Caller::view_scope`]; `None`, all.
    /// Refreshed at each recheck.
    clients: Option<Vec<String>>,
    /// The upgrade request's, with its cookie or token.
    headers: HeaderMap,
}

/// `GET /api/live`: upgrades to the websocket the module documentation describes.
pub(super) async fn live(
    axum::extract::State(state): axum::extract::State<State>,
    caller: Caller,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let upgrade = upgrade.map_err(|e| ApiError::new(e.status(), e.body_text()))?;
    let service = state.service()?;
    let counted = state.live.open(&caller.user)?;
    let events = Events {
        gateway: service.gateway().subscribe(),
        clients: service.client_events(),
        venues: service.venue_events(),
    };
    let viewer = Viewer { user: caller.user, ip: caller.ip, clients: caller.clients, headers };
    let upgrade = upgrade.max_message_size(MAX_RECEIVED_LEN).max_frame_size(MAX_RECEIVED_LEN);
    // A client that goes before the upgrade completes drops the closure, and with it the count.
    Ok(upgrade.on_upgrade(move |socket| run(socket, state, events, viewer, counted)))
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "snapshot")]
struct Snapshot {
    sessions: Vec<SessionView>,
    /// The working orders, by OrderId: a page of them at most ([`MAX_ORDERS_PER_PAGE`]).
    orders: Vec<OrderView>,
    /// With a full page, the `after` for `GET /api/orders?working=true` to read on from, as its
    /// `next` is; that may be an empty page.
    orders_next: Option<u64>,
    blocked: Vec<Scope>,
    unavailable: bool,
}

/// What happened to a session: a [`SessionEvent`].
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "session")]
struct SessionChange {
    side: SessionSide,
    id: String,
    /// `connected`, `logged_on`, `logging_out`, `resend_started`, `resend_finished`,
    /// `disconnected`, `paused`, `resumed` or `refused`.
    event: &'static str,
    /// Why a Logon was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// What the socket does next.
enum Step {
    Send(Message),
    /// A channel lagged: skip to its end and send a snapshot.
    Snapshot,
    /// The viewer's clients changed: as [`Snapshot`](Self::Snapshot), under the new scope. It
    /// comes once a [`RECHECK`] at most, so isn't held to [`MIN_SNAPSHOT_GAP`].
    Rescoped,
    Close(u16, &'static str),
    Nothing,
}

/// Sends a snapshot and then events until the server stops, the client goes, or the viewer's
/// credentials no longer hold; `_counted` stops counting the socket when it returns.
async fn run(mut socket: WebSocket, state: State, mut events: Events, mut viewer: Viewer, _counted: Counted) {
    info!(user = viewer.user, ip = %viewer.ip, "API live socket opened");
    let mut stopping = state.stopping.subscribe();
    let mut recheck = tokio::time::interval_at(Instant::now() + RECHECK, RECHECK);
    // A tick missed while a send waited comes once, late, not as a burst of rechecks and pings.
    recheck.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Its pongs, at least, every RECHECK.
    let mut last_received = Instant::now();
    let mut last_snapshot: Option<Instant> = None;
    let mut step = Step::Snapshot;
    let why = loop {
        step = match step {
            Step::Snapshot if last_snapshot.is_some_and(|last| last.elapsed() < MIN_SNAPSHOT_GAP) => {
                Step::Close(close_code::AGAIN, TOO_SLOW)
            }
            Step::Snapshot | Step::Rescoped => {
                events.skip_to_ends();
                last_snapshot = Some(Instant::now());
                snapshot(&state, viewer.clients.as_deref())
            }
            step => step,
        };
        match step {
            Step::Send(message) => {
                if !matches!(tokio::time::timeout(SEND_WAIT, socket.send(message)).await, Ok(Ok(()))) {
                    break "the client stopped reading";
                }
            }
            Step::Close(code, reason) => {
                let frame = CloseFrame { code, reason: reason.into() };
                let _ = tokio::time::timeout(SEND_WAIT, socket.send(Message::Close(Some(frame)))).await;
                break reason;
            }
            Step::Snapshot | Step::Rescoped | Step::Nothing => {}
        }
        step = tokio::select! {
            _ = stopping.wait_for(|stopping| *stopping) => Step::Close(close_code::AWAY, STOPPING),
            _ = recheck.tick() => {
                rechecked(&state, &mut viewer, last_received.elapsed() >= RECHECK * MAX_SILENT_TICKS)
            }
            event = events.gateway.recv() => match event {
                Ok(event) if shows(viewer.clients.as_deref(), &event) => json(&event),
                Ok(_) => Step::Nothing,
                Err(RecvError::Lagged(_)) => Step::Snapshot,
                Err(RecvError::Closed) => Step::Close(close_code::AWAY, STOPPING),
            },
            event = events.clients.recv() => match event {
                Ok(event) if viewer.clients.as_deref().is_none_or(|ids| is_one_of(&event.id.target_comp_id, ids)) => {
                    session_change(SessionSide::Client, &event).map_or(Step::Nothing, |change| json(&change))
                }
                Ok(_) => Step::Nothing,
                Err(RecvError::Lagged(_)) => Step::Snapshot,
                Err(RecvError::Closed) => Step::Close(close_code::AWAY, STOPPING),
            },
            event = events.venues.recv() => match event {
                Ok(event) => session_change(SessionSide::Venue, &event).map_or(Step::Nothing, |change| json(&change)),
                Err(RecvError::Lagged(_)) => Step::Snapshot,
                Err(RecvError::Closed) => Step::Close(close_code::AWAY, STOPPING),
            },
            message = socket.recv() => match message {
                Some(Ok(_)) => {
                    last_received = Instant::now();
                    Step::Nothing
                }
                None | Some(Err(_)) => break "the client went",
            },
        };
    };
    info!(user = viewer.user, ip = %viewer.ip, why, "API live socket closed");
}

/// What a recheck does: closes the socket if the viewer's credentials no longer hold or its client
/// is `silent`, sends a snapshot under the viewer's new scope if its clients changed, and pings.
fn rechecked(state: &State, viewer: &mut Viewer, silent: bool) -> Step {
    match state.auth.recheck(&viewer.headers, viewer.ip) {
        Err(refusal) => Step::Close(close_code::POLICY, refusal.message()),
        Ok(_) if silent => Step::Close(close_code::AWAY, SILENT),
        Ok(caller) if caller.clients != viewer.clients => {
            viewer.clients = caller.clients;
            Step::Rescoped
        }
        Ok(_) => Step::Send(Message::Ping(Default::default())),
    }
}

/// The snapshot, for a viewer of the clients of `comp_ids` (`None`, all), as JSON, or the close if
/// the gateway has gone.
fn snapshot(state: &State, comp_ids: Option<&[String]>) -> Step {
    let Ok(service) = state.service() else { return Step::Close(close_code::AWAY, STOPPING) };
    let narrowed = comp_ids.map(|ids| ids.iter().map(|id| id.as_str().into()).collect());
    let (orders, mut blocked, unavailable) = service.gateway().working_view(narrowed);
    if let Some(comp_ids) = comp_ids {
        blocked.retain(|scope| scope.concerns(comp_ids));
    }
    let orders_next = if orders.len() == MAX_ORDERS_PER_PAGE { orders.last().map(|order| order.id.0) } else { None };
    let sessions = session_views(&service, comp_ids);
    json(&Snapshot { sessions, orders, orders_next, blocked, unavailable })
}

/// Whether a viewer of the clients of `comp_ids` (`None`, all) is sent `event`: its clients'
/// orders, the blocks covering any of them, and the alarms
/// [`visible_to`](crate::book::Alarm::visible_to) it.
fn shows(comp_ids: Option<&[String]>, event: &GatewayEvent) -> bool {
    let Some(comp_ids) = comp_ids else { return true };
    match event {
        GatewayEvent::Order(order) => is_one_of(order.client.comp_id(), comp_ids),
        GatewayEvent::Blocked { scope } | GatewayEvent::Unblocked { scope } => scope.concerns(comp_ids),
        GatewayEvent::Alarm(alarm) => alarm.visible_to(Some(comp_ids)),
    }
}

fn json(value: &impl Serialize) -> Step {
    let text = serde_json::to_string(value).expect("the views serialize: their maps have string keys");
    Step::Send(Message::Text(text.into()))
}

/// `event` as the socket sends it; `None` for a kind newer than this code, which isn't sent.
fn session_change(side: SessionSide, event: &SessionEvent) -> Option<SessionChange> {
    let (name, reason) = match &event.kind {
        SessionEventKind::Connected => ("connected", None),
        SessionEventKind::LoggedOn => ("logged_on", None),
        SessionEventKind::LoggingOut => ("logging_out", None),
        SessionEventKind::ResendStarted => ("resend_started", None),
        SessionEventKind::ResendFinished => ("resend_finished", None),
        SessionEventKind::Disconnected => ("disconnected", None),
        SessionEventKind::Paused => ("paused", None),
        SessionEventKind::Resumed => ("resumed", None),
        SessionEventKind::Refused(reason) => ("refused", Some(reason.clone())),
        _ => return None,
    };
    Some(SessionChange { side, id: event.id.to_string(), event: name, reason })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::http::StatusCode;
    use futures_util::StreamExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

    use super::super::read::tests::{Fixture, fixture};
    use super::super::tests::send;
    use super::*;

    type Client = WebSocketStream<TcpStream>;

    /// Serves `app` on loopback, returning its address and what stops it.
    async fn serve(app: axum::Router) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        super::super::spawn(super::super::listener::Plain::new(tcp), app, stopped);
        (addr, stop)
    }

    /// Opens `path` on `addr` as a websocket, with `headers`.
    async fn connect(
        addr: SocketAddr,
        path: &str,
        headers: &[(&'static str, &str)],
    ) -> Result<Client, tungstenite::Error> {
        let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(*name, value.parse().unwrap());
        }
        let tcp = TcpStream::connect(addr).await.unwrap();
        tokio_tungstenite::client_async(request, tcp).await.map(|(client, _)| client)
    }

    /// The next message, which must come soon.
    async fn next(client: &mut Client) -> tungstenite::Message {
        tokio::time::timeout(Duration::from_secs(5), client.next()).await.expect("nothing sent").unwrap().unwrap()
    }

    /// The next text message, as JSON.
    async fn next_json(client: &mut Client) -> serde_json::Value {
        match next(client).await {
            tungstenite::Message::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    /// Reads until a message matching `found`, which must come soon.
    async fn until(client: &mut Client, found: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        loop {
            let message = next_json(client).await;
            if found(&message) {
                return message;
            }
        }
    }

    /// Serves [`run`] with `events` and `viewer`, as [`live`] would after its checks, and opens it.
    async fn open_run(state: State, events: Events, viewer: Viewer) -> (Client, tokio::sync::oneshot::Sender<()>) {
        let taken = Arc::new(Mutex::new(Some((events, viewer))));
        let route = axum::routing::get(async move |upgrade: WebSocketUpgrade| {
            let (events, viewer) = taken.lock().unwrap().take().unwrap();
            let counted = state.live.open(&viewer.user).unwrap();
            upgrade.on_upgrade(move |socket| run(socket, state, events, viewer, counted))
        });
        let (addr, stop) = serve(axum::Router::new().route("/test/live", route)).await;
        (connect(addr, "/test/live", &[]).await.unwrap(), stop)
    }

    /// Channels holding `capacity` events each, and the gateway's sender.
    fn channels(capacity: usize) -> (broadcast::Sender<GatewayEvent>, Events, [broadcast::Sender<SessionEvent>; 2]) {
        let (gateway, _) = broadcast::channel(capacity);
        let (clients, _) = broadcast::channel(capacity);
        let (venues, _) = broadcast::channel(capacity);
        let events = Events { gateway: gateway.subscribe(), clients: clients.subscribe(), venues: venues.subscribe() };
        (gateway, events, [clients, venues])
    }

    /// `user`'s viewer, scoped to `clients` if given, authenticating with `headers`.
    fn viewer(user: &str, clients: Option<&[&str]>, headers: HeaderMap) -> Viewer {
        let clients = clients.map(|clients| clients.iter().map(ToString::to_string).collect());
        Viewer { user: user.into(), ip: [127, 0, 0, 1].into(), clients, headers }
    }

    async fn open(fixture: &Fixture) -> (Client, SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let (addr, stop) = serve(fixture.app.clone()).await;
        let client = connect(addr, "/api/live", &[("authorization", &fixture.viewer)]).await.unwrap();
        (client, addr, stop)
    }

    /// The snapshot comes first: both sessions, the working order, no blocks.
    #[tokio::test]
    async fn a_snapshot_comes_first() {
        let fixture = fixture().await;
        let (mut client, _addr, _stop) = open(&fixture).await;
        let snapshot = next_json(&mut client).await;
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["sessions"].as_array().unwrap().len(), 2, "{snapshot}");
        assert_eq!(snapshot["sessions"][0]["status"], "logged_on");
        let [order] = snapshot["orders"].as_array().unwrap().as_slice() else { panic!("{snapshot}") };
        assert_eq!(order["status"], "pending_new");
        assert_eq!(snapshot["orders_next"], serde_json::Value::Null);
        assert_eq!(snapshot["blocked"], serde_json::json!([]));
        assert_eq!(snapshot["unavailable"], false);
    }

    /// An order the gateway cancels arrives as it changes; so does a block.
    #[tokio::test]
    async fn an_orders_events_arrive() {
        let fixture = fixture().await;
        let (mut client, _addr, _stop) = open(&fixture).await;
        let snapshot = next_json(&mut client).await;
        let id = snapshot["orders"][0]["id"].as_u64().unwrap();
        let gateway = fixture.service.gateway();
        gateway.cancel_order(crate::book::OrderId(id)).await.unwrap();
        let event = until(&mut client, |message| message["type"] == "order").await;
        assert_eq!(event["id"], id);
        assert_ne!(event, snapshot["orders"][0]);
        assert!(gateway.block(Scope::All).await);
        let blocked = until(&mut client, |message| message["type"] == "blocked").await;
        assert_eq!(blocked, serde_json::json!({ "type": "blocked", "scope": "all" }));
    }

    /// A client session paused and resumed arrives step by step, to its logging on again.
    #[tokio::test]
    async fn a_session_logging_on_arrives() {
        let fixture = fixture().await;
        let (mut client, _addr, _stop) = open(&fixture).await;
        next_json(&mut client).await;
        let id = fixture.service.client_statuses()[0].id.clone();
        let handle = fixture.service.client_session(&id);
        handle.pause(None);
        let paused = until(&mut client, |message| message["type"] == "session").await;
        let expected =
            serde_json::json!({ "type": "session", "side": "client", "id": id.to_string(), "event": "paused" });
        assert_eq!(paused, expected);
        handle.resume();
        until(&mut client, |message| message["event"] == "logged_on").await;
    }

    /// A venue's session arrives too: NASDAQ, down, paused and resumed.
    #[tokio::test]
    async fn a_venues_session_arrives() {
        let fixture = fixture().await;
        let (mut client, _addr, _stop) = open(&fixture).await;
        next_json(&mut client).await;
        let handle = fixture.service.venue_session("NASDAQ").unwrap();
        handle.pause(None);
        let paused = until(&mut client, |message| message["type"] == "session").await;
        let id = handle.id().to_string();
        assert_eq!(paused, serde_json::json!({ "type": "session", "side": "venue", "id": id, "event": "paused" }));
        handle.resume();
        let resumed = until(&mut client, |message| message["type"] == "session").await;
        assert_eq!(resumed["event"], "resumed");
    }

    /// A socket that falls behind gets a snapshot and skips what the channel held; falling behind
    /// again within MIN_SNAPSHOT_GAP closes it. Here the channels hold one event, and two are sent
    /// with no await between them, so the socket's task can't read in between.
    #[tokio::test]
    async fn lag_gives_a_snapshot() {
        let fixture = fixture().await;
        let (gateway, events, _sessions) = channels(1);
        let viewer = viewer("vic", None, HeaderMap::new());
        let (mut client, _stop) = open_run(fixture.state.clone(), events, viewer).await;
        assert_eq!(next_json(&mut client).await["type"], "snapshot");
        tokio::time::sleep(MIN_SNAPSHOT_GAP).await;
        let lag = || {
            gateway.send(GatewayEvent::Blocked { scope: Scope::All }).unwrap();
            gateway.send(GatewayEvent::Unblocked { scope: Scope::All }).unwrap();
        };
        lag();
        assert_eq!(next_json(&mut client).await["type"], "snapshot");
        // The `unblocked` the channel held is skipped: the next is the one sent after.
        gateway.send(GatewayEvent::Blocked { scope: Scope::All }).unwrap();
        assert_eq!(next_json(&mut client).await["type"], "blocked");
        lag();
        let tungstenite::Message::Close(Some(frame)) = next(&mut client).await else { panic!("not closed") };
        assert_eq!(u16::from(frame.code), close_code::AGAIN);
    }

    /// FUNDA's viewer's snapshot holds FUNDA's order, block and session, and the venue's; FUNDB's
    /// operator's, FUNDB's and the venue's. An unscoped viewer's holds each.
    #[tokio::test]
    async fn a_scoped_viewers_snapshot_is_its_clients() {
        let fixture = fixture().await;
        let gateway = fixture.service.gateway();
        crate::edge::tests::book_input(gateway, crate::edge::tests::new_order_from("FUNDB", "B1"));
        for comp_id in ["FUNDA", "FUNDB"] {
            assert!(gateway.block(Scope::CompId(comp_id.into())).await);
        }
        let (addr, _stop) = serve(fixture.app.clone()).await;
        let snapshot_as = async |bearer: &str| {
            let mut client = connect(addr, "/api/live", &[("authorization", bearer)]).await.unwrap();
            let snapshot = next_json(&mut client).await;
            let column = |name: &str, field: &str| -> Vec<_> {
                snapshot[name].as_array().unwrap().iter().map(|row| row[field].as_str().unwrap().to_string()).collect()
            };
            (column("orders", "client"), snapshot["blocked"].clone(), column("sessions", "id"))
        };
        let client = |comp_id: &str| format!("FIX.4.4:JETBRIDGE->{comp_id}");
        let venue = fixture.service.venue_session("NASDAQ").unwrap().id().to_string();
        let all = (vec![client("FUNDA"), client("FUNDB")], serde_json::json!(["comp:FUNDA", "comp:FUNDB"]));
        assert_eq!(snapshot_as(&fixture.viewer).await, (all.0, all.1, vec![client("FUNDA"), venue.clone()]));
        let funda = (vec![client("FUNDA")], serde_json::json!(["comp:FUNDA"]), vec![client("FUNDA"), venue.clone()]);
        assert_eq!(snapshot_as(&fixture.scoped_viewer).await, funda);
        let fundb = (vec![client("FUNDB")], serde_json::json!(["comp:FUNDB"]), vec![venue]);
        assert_eq!(snapshot_as(&fixture.scoped).await, fundb);
    }

    /// FUNDA's viewer gets FUNDA's orders, blocks and alarms, blocks of all and the gateway's
    /// alarms; not FUNDB's, nor an orphan's. An unscoped viewer is sent each.
    #[tokio::test]
    async fn a_scoped_viewer_gets_its_clients_events() {
        let fixture = fixture().await;
        let (gateway, events, _sessions) = channels(16);
        let (mut client, _stop) =
            open_run(fixture.state.clone(), events, viewer("val", Some(&["FUNDA"]), HeaderMap::new())).await;
        assert_eq!(next_json(&mut client).await["type"], "snapshot");
        let funda = fixture.service.gateway().working_view(None).0.remove(0);
        let fundb = OrderView { client: "FUNDB".into(), ..funda.clone() };
        let alarm = |kind, client: Option<&str>| {
            let client = client.map(Into::into);
            GatewayEvent::Alarm(crate::book::Alarm { time: chrono::Utc::now(), kind, client, text: String::new() })
        };
        let comp = |comp_id: &str| Scope::CompId(comp_id.into());
        use crate::book::AlarmKind::{GatewayUnavailable, Orphan, StuckCancel, UnknownOrder};
        // Each held back, then one sent: one channel keeps their order, so the next to arrive is
        // the one sent.
        let cases = [
            (GatewayEvent::Order(fundb), GatewayEvent::Order(funda)),
            (GatewayEvent::Blocked { scope: comp("FUNDB") }, GatewayEvent::Blocked { scope: comp("FUNDA") }),
            (
                GatewayEvent::Unblocked { scope: Scope::Session("FUNDB".into()) },
                GatewayEvent::Unblocked { scope: Scope::All },
            ),
            (alarm(StuckCancel, Some("FUNDB")), alarm(StuckCancel, Some("FUNDA"))),
            (alarm(Orphan, None), alarm(GatewayUnavailable, None)),
            (alarm(UnknownOrder, None), alarm(GatewayUnavailable, None)),
        ];
        for (held, sent) in cases {
            assert!(shows(None, &held), "{held:?}");
            gateway.send(held).unwrap();
            gateway.send(sent.clone()).unwrap();
            assert_eq!(next_json(&mut client).await, serde_json::to_value(&sent).unwrap());
        }
    }

    /// FUNDB's operator isn't told of FUNDA's session pausing, as FUNDA's viewer is; both are told
    /// of the venue's.
    #[tokio::test]
    async fn a_scoped_viewer_gets_its_clients_sessions() {
        let fixture = fixture().await;
        let (addr, _stop) = serve(fixture.app.clone()).await;
        let mut funda = connect(addr, "/api/live", &[("authorization", &fixture.scoped_viewer)]).await.unwrap();
        let mut fundb = connect(addr, "/api/live", &[("authorization", &fixture.scoped)]).await.unwrap();
        next_json(&mut funda).await;
        next_json(&mut fundb).await;
        fixture.service.client_session(&fixture.service.client_statuses()[0].id).pause(None);
        assert_eq!(until(&mut funda, |message| message["type"] == "session").await["side"], "client");
        fixture.service.venue_session("NASDAQ").unwrap().pause(None);
        assert_eq!(until(&mut fundb, |message| message["type"] == "session").await["side"], "venue");
        // FUNDA's pausing was sent to the socket first, and is held back: nothing more comes.
        let more = tokio::time::timeout(Duration::from_millis(200), fundb.next()).await;
        assert!(more.is_err(), "{more:?}");
    }

    /// A user's clients changed by a reload take effect at the socket's next recheck: a snapshot
    /// under the new scope, then its events alone.
    #[tokio::test]
    async fn a_scope_change_takes_effect_at_the_recheck() {
        let fixture = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.toml");
        let mut users = crate::auth::Users::default();
        users.add("val", crate::auth::Role::Viewer, vec!["FUNDA".into()], "a long password").unwrap();
        let token = users.add_token("val").unwrap();
        users.save(&path).unwrap();
        let mut state = fixture.state.clone();
        state.auth = Arc::new(super::super::auth::Auth::new(path.clone(), users, false, false));
        let headers =
            HeaderMap::from_iter([(axum::http::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap())]);
        let (gateway, events, _sessions) = channels(16);
        let (mut client, _stop) = open_run(state.clone(), events, viewer("val", Some(&["FUNDA"]), headers)).await;
        assert_eq!(next_json(&mut client).await["orders"].as_array().unwrap().len(), 1);

        let text = std::fs::read_to_string(&path).unwrap();
        let changed = text.replace(r#"clients = ["FUNDA"]"#, r#"clients = ["FUNDB"]"#);
        assert_ne!(changed, text);
        std::fs::write(&path, changed).unwrap();
        assert_eq!(state.auth.reload(), None);
        tokio::time::pause();
        tokio::time::advance(RECHECK).await;
        let snapshot = next_json(&mut client).await;
        assert_eq!(snapshot["type"], "snapshot");
        assert_eq!(snapshot["orders"], serde_json::json!([]));
        let funda = fixture.service.gateway().working_view(None).0.remove(0);
        let fundb = OrderView { client: "FUNDB".into(), ..funda.clone() };
        gateway.send(GatewayEvent::Order(funda)).unwrap();
        gateway.send(GatewayEvent::Order(fundb.clone())).unwrap();
        assert_eq!(next_json(&mut client).await, serde_json::to_value(GatewayEvent::Order(fundb)).unwrap());
    }

    /// A user may have MAX_LIVE_SOCKETS_PER_USER sockets open, 429 past that; there may be
    /// MAX_LIVE_SOCKETS in all, 503 past that. A socket closed is no longer counted.
    #[tokio::test]
    async fn live_sockets_are_capped() {
        let fixture = fixture().await;
        let (addr, _stop) = serve(fixture.app.clone()).await;
        let mut open = Vec::new();
        for _ in 0..MAX_LIVE_SOCKETS_PER_USER {
            open.push(connect(addr, "/api/live", &[("authorization", &fixture.viewer)]).await.unwrap());
        }
        let Err(tungstenite::Error::Http(answer)) =
            connect(addr, "/api/live", &[("authorization", &fixture.viewer)]).await
        else {
            panic!("over the cap per user")
        };
        assert_eq!(answer.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(connect(addr, "/api/live", &[("authorization", &fixture.operator)]).await.is_ok());

        let sockets = Arc::new(Sockets::default());
        let mut counted: Vec<_> =
            (0..MAX_LIVE_SOCKETS).map(|n| sockets.open(&format!("user{}", n % 9)).unwrap()).collect();
        assert_eq!(sockets.open("someone").err().unwrap().status, StatusCode::SERVICE_UNAVAILABLE);
        counted.pop();
        assert!(sockets.open("someone").is_ok());
        drop(counted);
        assert_eq!(sockets.lock().total, 0);
        assert!(sockets.lock().by_user.is_empty());
    }

    /// Stopping the server closes the socket, saying why.
    #[tokio::test]
    async fn stopping_closes_the_socket() {
        let fixture = fixture().await;
        let (mut client, _addr, _stop) = open(&fixture).await;
        next_json(&mut client).await;
        fixture.state.stopping.send_replace(true);
        let tungstenite::Message::Close(Some(frame)) = next(&mut client).await else { panic!("not closed") };
        assert_eq!(u16::from(frame.code), close_code::AWAY);
        assert_eq!(frame.reason.as_str(), STOPPING);
    }

    /// A recheck's refusal is a close reason too, so it must fit one; the socket's own reasons are
    /// checked at compile time.
    #[test]
    fn refusals_fit_a_close_frame() {
        use super::super::auth::Refusal;
        let refusals = [
            Refusal::NoCredentials,
            Refusal::BadPassword,
            Refusal::BadToken,
            Refusal::SessionEnded,
            Refusal::Csrf,
            Refusal::Origin,
            Refusal::Limited,
        ];
        for refusal in refusals {
            assert!(refusal.message().len() <= MAX_CLOSE_REASON_LEN, "{refusal:?}");
        }
    }

    /// With the cookie, the upgrade must come from a page on the host it was sent to; with a token,
    /// no Origin is needed. A request that isn't an upgrade is refused.
    #[tokio::test]
    async fn the_cookie_needs_this_hosts_origin() {
        let fixture = fixture().await;
        let login =
            send(&fixture.app, "POST", "/api/login", &[], r#"{"user":"vic","password":"a long password"}"#).await;
        let set = login.headers["set-cookie"].to_str().unwrap();
        let cookie = set.split(';').next().unwrap();
        let (addr, _stop) = serve(fixture.app.clone()).await;
        let here = format!("http://{addr}");
        for origin in [None, Some("http://evil.example"), Some("null")] {
            let headers: Vec<_> = [("cookie", cookie)].into_iter().chain(origin.map(|o| ("origin", o))).collect();
            let Err(tungstenite::Error::Http(answer)) = connect(addr, "/api/live", &headers).await else {
                panic!("{origin:?} upgraded")
            };
            assert_eq!(answer.status(), StatusCode::FORBIDDEN, "{origin:?}");
        }
        let mut client = connect(addr, "/api/live", &[("cookie", cookie), ("origin", &here)]).await.unwrap();
        assert_eq!(next_json(&mut client).await["type"], "snapshot");
        assert_eq!(fixture.get("/api/live").await.status, StatusCode::BAD_REQUEST);
    }
}
