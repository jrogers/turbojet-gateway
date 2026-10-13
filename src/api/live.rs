//! `GET /api/live` [viewer]: a websocket of what changes, for a console. It sends a snapshot,
//! `{"type":"snapshot", sessions, orders, orders_next, blocked, unavailable}`, then each event as
//! it happens: the gateway's (`order`, `blocked`, `unblocked` and `alarm`; see [`GatewayEvent`])
//! and the client sessions' (`session`). What the client sends is ignored.
//!
//! The events are subscribed to before the snapshot is read, so none falls between the two; the
//! first few may repeat what the snapshot already shows. A socket that falls behind either
//! channel's capacity gets a new snapshot and carries on, rather than the gateway waiting on it:
//! the channels bound what a slow socket costs.
//!
//! The upgrade authenticates as any request does, with the cookie or a bearer token, never a token
//! in the URL, which proxies log. With the cookie its `Origin` must be the API's own host (see
//! [`auth`](super::auth)). The socket closes when the server stops, and within [`RECHECK`] of its
//! session ending or its token being revoked. Like other reads, it isn't audited.

use std::net::IpAddr;
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;
use tracing::info;
use turbojet::{SessionEvent, SessionEventKind};

use super::read::{SessionSide, SessionView, session_views};
use super::{ApiError, Caller, State};
use crate::book::{GatewayEvent, MAX_ORDERS_PER_PAGE, OrderFilter, OrderView, Scope};

/// How often an open socket checks that its session or token still holds. Checking is a map
/// lookup; a removed user or a revoked token keeps watching this long at most.
const RECHECK: Duration = Duration::from_secs(10);

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
}

/// Whose socket it is, and what its rechecks authenticate.
pub(super) struct Viewer {
    user: String,
    ip: IpAddr,
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
    let events = Events { gateway: service.gateway().subscribe(), clients: service.client_events() };
    let viewer = Viewer { user: caller.user, ip: caller.ip, headers };
    let upgrade = upgrade.max_message_size(MAX_RECEIVED_LEN).max_frame_size(MAX_RECEIVED_LEN);
    Ok(upgrade.on_upgrade(move |socket| run(socket, state, events, viewer)))
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
    Send(String),
    Close(u16, &'static str),
    Nothing,
}

/// Sends a snapshot and then events until the server stops, the client goes, or the viewer's
/// credentials no longer hold.
async fn run(mut socket: WebSocket, state: State, mut events: Events, viewer: Viewer) {
    info!(user = viewer.user, ip = %viewer.ip, "API live socket opened");
    let mut stopping = state.stopping.subscribe();
    let mut recheck = tokio::time::interval_at(Instant::now() + RECHECK, RECHECK);
    let mut step = snapshot(&state);
    let why = loop {
        match step {
            Step::Send(text) => {
                if !matches!(tokio::time::timeout(SEND_WAIT, socket.send(Message::Text(text.into()))).await, Ok(Ok(())))
                {
                    break "the client stopped reading";
                }
            }
            Step::Close(code, reason) => {
                let frame = CloseFrame { code, reason: reason.into() };
                let _ = tokio::time::timeout(SEND_WAIT, socket.send(Message::Close(Some(frame)))).await;
                break reason;
            }
            Step::Nothing => {}
        }
        step = tokio::select! {
            _ = stopping.wait_for(|stopping| *stopping) => Step::Close(close_code::AWAY, "the gateway is stopping"),
            _ = recheck.tick() => match state.auth.recheck(&viewer.headers, viewer.ip) {
                Ok(()) => Step::Nothing,
                Err(refusal) => Step::Close(close_code::POLICY, refusal.message()),
            },
            event = events.gateway.recv() => match event {
                Ok(event) => json(&event),
                Err(RecvError::Lagged(_)) => snapshot(&state),
                Err(RecvError::Closed) => Step::Close(close_code::AWAY, "the gateway is stopping"),
            },
            event = events.clients.recv() => match event {
                Ok(event) => session_change(SessionSide::Client, &event).map_or(Step::Nothing, |change| json(&change)),
                Err(RecvError::Lagged(_)) => snapshot(&state),
                Err(RecvError::Closed) => Step::Close(close_code::AWAY, "the gateway is stopping"),
            },
            message = socket.recv() => match message {
                Some(Ok(_)) => Step::Nothing,
                None | Some(Err(_)) => break "the client went",
            },
        };
    };
    info!(user = viewer.user, ip = %viewer.ip, why, "API live socket closed");
}

/// The snapshot as JSON, or the close if the gateway has gone.
fn snapshot(state: &State) -> Step {
    let Ok(service) = state.service() else { return Step::Close(close_code::AWAY, "the gateway is stopping") };
    let gateway = service.gateway();
    let orders = gateway.orders(&OrderFilter { working: true, ..OrderFilter::default() });
    let orders_next = if orders.len() == MAX_ORDERS_PER_PAGE { orders.last().map(|order| order.id.0) } else { None };
    json(&Snapshot {
        sessions: session_views(&service),
        orders,
        orders_next,
        blocked: gateway.blocked(),
        unavailable: gateway.unavailable(),
    })
}

fn json(value: &impl Serialize) -> Step {
    Step::Send(serde_json::to_string(value).expect("the views serialize: their maps have string keys"))
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
        assert_eq!(next_json(&mut client).await, serde_json::json!({ "type": "blocked", "scope": "all" }));
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

    /// A socket that falls behind gets a snapshot, then what's still in the channel: here a
    /// channel of one, sent two events before the socket reads.
    #[tokio::test]
    async fn lag_gives_a_snapshot() {
        let fixture = fixture().await;
        let (gateway, _) = broadcast::channel(1);
        let (clients, _) = broadcast::channel(1);
        let events = Mutex::new(Some(Events { gateway: gateway.subscribe(), clients: clients.subscribe() }));
        let (state, headers) = (fixture.state.clone(), HeaderMap::new());
        let viewer = Mutex::new(Some(Viewer { user: "vic".into(), ip: [127, 0, 0, 1].into(), headers }));
        let take = Arc::new(move || (events.lock().unwrap().take().unwrap(), viewer.lock().unwrap().take().unwrap()));
        let route = axum::routing::get(async move |upgrade: WebSocketUpgrade| {
            let (events, viewer) = take();
            upgrade.on_upgrade(move |socket| run(socket, state, events, viewer))
        });
        let (addr, _stop) = serve(axum::Router::new().route("/test/live", route)).await;
        let mut client = connect(addr, "/test/live", &[]).await.unwrap();
        assert_eq!(next_json(&mut client).await["type"], "snapshot");
        // The test's runtime has one thread: the socket's task can't read between the sends.
        gateway.send(GatewayEvent::Blocked { scope: Scope::All }).unwrap();
        gateway.send(GatewayEvent::Unblocked { scope: Scope::All }).unwrap();
        assert_eq!(next_json(&mut client).await["type"], "snapshot");
        assert_eq!(next_json(&mut client).await["type"], "unblocked");
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
        assert_eq!(frame.reason.as_str(), "the gateway is stopping");
    }

    /// With the cookie, the upgrade must come from a page on the host it was sent to; with a token,
    /// no Origin is needed. A request that isn't an upgrade is refused.
    #[tokio::test]
    async fn the_cookie_needs_this_hosts_origin() {
        let fixture = fixture().await;
        let login = send(&fixture.app, "POST", "/api/login", &[], r#"{"user":"vic","password":"a password"}"#).await;
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
