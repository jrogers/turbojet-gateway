//! The gateway's edges: what connects the book to client and venue sessions.
//!
//! One [`Gateway`] holds the book. [`ClientApp`] serves the acceptor the clients log on to, and
//! [`VenueApp`] the initiators that log on to venues. Both call into the book inline, under its
//! lock, and send what it answers before letting go: reports through the [`Outbox`], venue
//! requests through each venue's [`SessionHandle`]. Neither waits, so the lock is held for
//! microseconds, and every client gets its reports in the order the book made them, even when
//! its own request and a venue's fill race on different threads.

pub mod outbox;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::error;
use turbojet::fields::{BusinessRejectReason, CompactString, UtcTimestamp};
use turbojet::{
    Application, Clock, ConnectionInfo, Context, Message, MessageReject, SendError, SessionHandle, SessionId,
};

use crate::book::{Book, ClientId, Input, Output, VenueId};
use crate::fix::{self, UnsupportedVersion, Version};
use crate::router::Router;
pub use outbox::Outbox;

/// Text(58) of the reject for an order whose venue isn't connected, or isn't configured.
const VENUE_UNAVAILABLE: &str = "venue unavailable";

/// Text(58) of the reject for an order whose venue's send queue is full.
const VENUE_BUSY: &str = "venue unavailable: its send queue is full";

/// The book, and where its outputs go.
#[derive(Debug)]
pub struct Gateway {
    state: Mutex<State>,
    outbox: Outbox,
    /// For TransactTime(60) on what the gateway sends.
    clock: Clock,
}

/// Everything sending needs, under one lock so reports leave in the book's order.
#[derive(Debug)]
struct State {
    book: Book,
    /// Each client's FIX version, recorded at logon, for the reports a venue's message makes.
    clients: HashMap<ClientId, Version>,
    venues: HashMap<VenueId, Venue>,
    /// Which venue each initiator's session is.
    venue_names: HashMap<SessionId, VenueId>,
}

#[derive(Debug)]
struct Venue {
    version: Version,
    session: SessionHandle,
}

impl Gateway {
    /// A gateway routing by `router`, with no venues until [`set_venues`](Self::set_venues).
    pub fn new(router: Router) -> Self {
        let state = State {
            book: Book::new(router),
            clients: HashMap::new(),
            venues: HashMap::new(),
            venue_names: HashMap::new(),
        };
        Self { state: Mutex::new(state), outbox: Outbox::new(), clock: Clock::system() }
    }

    /// Stamps TransactTime(60) from `clock` rather than the system's.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The venues, by name (`[initiator.NAME]`), each with its initiator's session; replaces any
    /// set before. Call it once the initiators are made, and again after a reload changes them.
    /// Until a venue is set, its session's logons are refused (see [`VenueApp`]) and orders for
    /// it are rejected as unavailable.
    ///
    /// # Errors
    ///
    /// A venue's session has a FIX version the gateway doesn't convert; nothing is changed.
    pub fn set_venues(
        &self,
        venues: impl IntoIterator<Item = (VenueId, SessionHandle)>,
    ) -> Result<(), UnsupportedVersion> {
        let mut by_name = HashMap::new();
        let mut names = HashMap::new();
        for (name, session) in venues {
            let version = session.id().begin_string.parse()?;
            names.insert(session.id().clone(), name.clone());
            by_name.insert(name, Venue { version, session });
        }
        let mut state = self.lock();
        state.venues = by_name;
        state.venue_names = names;
        Ok(())
    }

    /// Routes new orders by `router` from now on.
    pub fn set_router(&self, router: Router) {
        self.lock().book.set_router(router);
    }

    /// Runs `input` through the book and sends what it answers, all under the lock. A venue
    /// request that can't be queued goes back in as [`Input::SendFailed`], which never makes
    /// venue-bound output, so this ends after one more round per refused request.
    fn handle(&self, input: Input) {
        let now = UtcTimestamp::from(self.clock.now());
        let mut state = self.lock();
        let state = &mut *state;
        let mut inputs = VecDeque::from([input]);
        let mut outputs = Vec::new();
        while let Some(input) = inputs.pop_front() {
            let send_failed = matches!(input, Input::SendFailed { .. });
            state.book.handle(input, &mut outputs);
            for output in outputs.drain(..) {
                if let Some(failed) = self.send(state, &output, now) {
                    debug_assert!(!send_failed, "SendFailed made venue-bound output");
                    inputs.push_back(failed);
                }
            }
        }
    }

    /// Sends `output` where it's going. `Some` for a venue request its session refused.
    fn send(&self, state: &State, output: &Output, now: UtcTimestamp) -> Option<Input> {
        match output {
            Output::Report { client, .. } | Output::CancelReject { client, .. } => {
                // Every client the book answers sent it a request, so logged on first.
                let Some(version) = state.clients.get(client) else {
                    error!(%client, "report for a client that never logged on; dropped");
                    return None;
                };
                let msg = version.client_message(output, now).expect("a client-bound output");
                // Not `ctx.send`, even to the client whose request this answers: the outbox keeps
                // each client's reports in the book's order. If the process stops before this is
                // sent, at-least-once delivery covers it: the client's request comes again, and
                // the book answers the redelivery.
                self.outbox.deliver(client, msg);
                None
            }
            Output::NewOrder { venue, downstream, .. }
            | Output::Cancel { venue, downstream, .. }
            | Output::Replace { venue, downstream, .. } => {
                let failed = |text: &str| Input::SendFailed {
                    venue: venue.clone(),
                    downstream: downstream.clone(),
                    text: CompactString::from(text),
                };
                let Some(to) = state.venues.get(venue) else { return Some(failed(VENUE_UNAVAILABLE)) };
                let msg = to.version.venue_message(output, now).expect("a venue-bound output");
                // ponytail: the receipt is dropped, so a request the connection loses before
                // storing it leaves the order pending; stage 4 holds the client's commit on it.
                match to.session.send(msg) {
                    Ok(_receipt) => None,
                    Err(SendError::Full(_)) => Some(failed(VENUE_BUSY)),
                    // NotConnected.
                    Err(_) => Some(failed(VENUE_UNAVAILABLE)),
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic inside the book may have left it half-updated: stop trading rather than go on.
        self.state.lock().expect("the book panicked; no more orders")
    }
}

/// The acceptor's application: clients, each named by its CompID (the session's TargetCompID).
#[derive(Debug, Clone)]
pub struct ClientApp(pub Arc<Gateway>);

impl Application for ClientApp {
    fn verify_logon(&self, session: &SessionHandle, _logon: &Message, _: &ConnectionInfo) -> Result<(), String> {
        session.id().begin_string.parse::<Version>().map(drop).map_err(|e| e.to_string())
    }

    fn on_logon(&self, session: &SessionHandle) {
        let id = session.id();
        let version = id.begin_string.parse().expect("checked in verify_logon");
        self.0.lock().clients.insert(ClientId::from(id.target_comp_id.as_str()), version);
        self.0.outbox.logged_on(session);
    }

    fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let id = ctx.session_id();
        let version: Version = id.begin_string.parse().expect("checked in verify_logon");
        let client = ClientId::from(id.target_comp_id.as_str());
        let redelivered = ctx.maybe_redelivered() || fix::possibly_resent(msg);
        let input = version.client_input(&client, msg, redelivered)?;
        self.0.handle(input);
        Ok(())
    }
}

/// The initiators' application: each venue is its `[initiator.NAME]` name, as given to
/// [`Gateway::set_venues`]. A logon on a session not set there is refused, so no venue message
/// arrives before the gateway knows whose it is: the initiators start before the gateway can be
/// told their sessions, and the initiator's reconnect policy retries a refused logon.
#[derive(Debug, Clone)]
pub struct VenueApp(pub Arc<Gateway>);

impl Application for VenueApp {
    fn verify_logon(&self, session: &SessionHandle, _logon: &Message, _: &ConnectionInfo) -> Result<(), String> {
        if self.0.lock().venue_names.contains_key(session.id()) {
            Ok(())
        } else {
            Err(format!("session {} isn't a venue the gateway knows yet", session.id()))
        }
    }

    fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let known = {
            let state = self.0.lock();
            let name = state.venue_names.get(ctx.session_id()).cloned();
            name.and_then(|name| state.venues.get(&name).map(|venue| (name, venue.version)))
        };
        // Only if a reload dropped the venue while its session was up.
        let Some((venue, version)) = known else {
            error!(session = %ctx.session_id(), "message from a venue no longer configured; rejected");
            let reason = BusinessRejectReason::ApplicationNotAvailable;
            return Err(MessageReject::Business { reason, text: VENUE_UNAVAILABLE.into() });
        };
        // `None`: a report the book needn't see (a bust, say), logged inside.
        if let Some(input) = version.venue_input(&venue, msg)? {
            self.0.handle(input);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tokio::time::timeout;
    use turbojet::fields::Decimal;
    use turbojet::message::tags;
    use turbojet::{
        Acceptor, FixMessage, Initiator, InitiatorConfig, MemoryStorage, MsgType, ReconnectPolicy, SessionConfig,
    };
    use turbojet_fix44 as v44;

    use super::*;
    use crate::router::{Rule, Target};

    const VENUE: &str = "NASDAQ";

    /// Every order to [`VENUE`].
    fn gateway() -> Arc<Gateway> {
        let rule = Rule { client: None, ex_destination: None, symbol: None, target: Target::Venue(VENUE.into()) };
        Arc::new(Gateway::new(Router::new(vec![rule])))
    }

    fn session(begin_string: &str, target: &str) -> SessionHandle {
        SessionHandle::disconnected(SessionId::new(begin_string, "JETBRIDGE", target))
    }

    fn logon(app: &impl Application, session: &SessionHandle) -> Result<(), String> {
        app.verify_logon(session, &Message::new(MsgType::Logon), &ConnectionInfo::new(None, Vec::new()))
    }

    #[test]
    fn a_client_logs_on_only_with_a_version_the_gateway_converts() {
        let app = ClientApp(gateway());
        assert_eq!(logon(&app, &session("FIX.4.2", "FUNDA")), Ok(()));
        assert_eq!(logon(&app, &session("FIX.4.4", "FUNDA")), Ok(()));
        let refused = logon(&app, &session("FIX.4.3", "FUNDA")).unwrap_err();
        assert!(refused.contains("FIX.4.3"), "{refused}");
    }

    #[test]
    fn a_venue_logs_on_only_once_the_gateway_knows_it() {
        let gateway = gateway();
        let (app, venue) = (VenueApp(gateway.clone()), session("FIX.4.4", "VENUE"));
        assert!(logon(&app, &venue).is_err());
        gateway.set_venues([(VENUE.into(), venue.clone())]).unwrap();
        assert_eq!(logon(&app, &venue), Ok(()));
        gateway.set_venues([]).unwrap();
        assert!(logon(&app, &venue).is_err());
    }

    #[test]
    fn a_venue_with_a_version_the_gateway_cant_convert_is_refused() {
        let gateway = gateway();
        let refused = gateway.set_venues([(VENUE.into(), session("FIX.4.3", "VENUE"))]);
        assert_eq!(refused, Err(UnsupportedVersion("FIX.4.3".into())));
    }

    #[test]
    fn a_request_that_doesnt_parse_is_rejected_by_the_session() {
        let app = ClientApp(gateway());
        let client = session("FIX.4.4", "FUNDA");
        // No Side(54).
        let order = Message::new(MsgType::NewOrderSingle).with(tags::CL_ORD_ID, "C1");
        let reject = app.on_message(&mut Context::new(&client), &order).unwrap_err();
        assert!(matches!(reject, MessageReject::Session { ref_tag: Some(tags::SIDE), .. }), "{reject:?}");
        let other = Message::new(MsgType::from_static("S"));
        assert_eq!(app.on_message(&mut Context::new(&client), &other), Err(MessageReject::unsupported_message_type()));
    }

    /// Something a test's session saw.
    #[derive(Debug)]
    enum Event {
        LoggedOn,
        Message(Message),
    }

    /// A client or venue in the test: reports what it sees, and as a venue, acks and fills every
    /// order in full at 10.
    struct Peer {
        events: UnboundedSender<Event>,
    }

    impl Application for Peer {
        fn on_logon(&self, _session: &SessionHandle) {
            let _ = self.events.send(Event::LoggedOn);
        }

        fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
            let _ = self.events.send(Event::Message(msg.clone()));
            if msg.msg_type() == MsgType::NewOrderSingle {
                let order: v44::NewOrderSingle = msg.parse()?;
                let qty = order.order_qty.expect("orders carry OrderQty");
                for (exec_type, status, last_qty) in
                    [(v44::ExecType::New, v44::OrdStatus::New, 0), (v44::ExecType::Trade, v44::OrdStatus::Filled, 100)]
                {
                    let (last_qty, px) = (Decimal::from(last_qty), Decimal::from(10));
                    let (exec_id, leaves_qty) = (format!("X{last_qty}"), qty - last_qty);
                    let mut report = v44::ExecutionReport::new(
                        "V-1", exec_id, exec_type, status, order.side, leaves_qty, last_qty, px,
                    );
                    report.cl_ord_id = Some(order.cl_ord_id.clone());
                    (report.last_qty, report.last_px) = (Some(last_qty), Some(px));
                    ctx.send(report);
                }
            }
            Ok(())
        }
    }

    fn peer() -> (Arc<Peer>, UnboundedReceiver<Event>) {
        let (events, rx) = unbounded_channel();
        (Arc::new(Peer { events }), rx)
    }

    async fn next(events: &mut UnboundedReceiver<Event>) -> Event {
        timeout(Duration::from_secs(5), events.recv()).await.expect("timed out").expect("closed")
    }

    async fn next_message(events: &mut UnboundedReceiver<Event>) -> Message {
        match next(events).await {
            Event::Message(msg) => msg,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// Serves `app` as `comp_id` on loopback; its address.
    async fn serve(comp_id: &str, app: Arc<dyn Application>) -> String {
        let config = SessionConfig::new("FIX.4.4", comp_id);
        let acceptor = Acceptor::new(config, Arc::new(MemoryStorage::new()), app).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(acceptor.serve(listener));
        addr
    }

    /// An initiator from `comp_id` to `target` at `addr`, serving `app`, not yet running.
    fn initiator(addr: &str, comp_id: &str, target: &str, app: Arc<dyn Application>) -> Initiator {
        let mut config = InitiatorConfig::new(SessionConfig::new("FIX.4.4", comp_id), target);
        config.reconnect = ReconnectPolicy::fixed(Duration::from_millis(50));
        Initiator::new(addr, config, Arc::new(MemoryStorage::new()), app).unwrap()
    }

    /// Jetbridge serving clients with `gateway`, and a client logged on to it.
    async fn client(gateway: &Arc<Gateway>) -> (Initiator, UnboundedReceiver<Event>) {
        let addr = serve("JETBRIDGE", Arc::new(ClientApp(gateway.clone()))).await;
        let (app, mut events) = peer();
        let client = initiator(&addr, "FUNDA", "JETBRIDGE", app);
        tokio::spawn(client.clone().run());
        assert!(matches!(next(&mut events).await, Event::LoggedOn));
        (client, events)
    }

    fn order(cl_ord_id: &str) -> Message {
        let now = UtcTimestamp::now();
        let mut order = v44::NewOrderSingle::new(cl_ord_id, v44::Side::Buy, now, v44::OrdType::Limit);
        (order.symbol, order.order_qty, order.price) = (Some("AAPL".into()), Some(100.into()), Some(10.into()));
        order.to_message()
    }

    async fn report(events: &mut UnboundedReceiver<Event>) -> v44::ExecutionReport {
        next_message(events).await.parse().unwrap()
    }

    /// The client's order reaches the venue under our ClOrdID, and the venue's ack and fill come
    /// back to the client under its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_order_goes_to_the_venue_and_its_reports_come_back() {
        let gateway = gateway();
        let (venue_app, mut venue_events) = peer();
        let venue_addr = serve("VENUE", venue_app).await;
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        gateway.set_venues([(VENUE.into(), venue.handle())]).unwrap();
        tokio::spawn(venue.clone().run());
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        let (client, mut client_events) = client(&gateway).await;

        client.handle().send(order("C1")).unwrap();
        let sent: v44::NewOrderSingle = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "1");
        let new = report(&mut client_events).await;
        assert_eq!(
            (new.cl_ord_id.as_deref(), new.exec_type, new.ord_status),
            (Some("C1"), v44::ExecType::New, v44::OrdStatus::New)
        );
        let fill = report(&mut client_events).await;
        assert_eq!((fill.cl_ord_id.as_deref(), fill.exec_type), (Some("C1"), v44::ExecType::Trade));
        assert_eq!((fill.last_qty, fill.cum_qty, fill.leaves_qty), (Some(100.into()), 100.into(), 0.into()));
    }

    /// With the venue's initiator not connected, the order is rejected rather than left pending.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_order_for_a_venue_not_connected_is_rejected() {
        let gateway = gateway();
        let venue = initiator("127.0.0.1:1", "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        gateway.set_venues([(VENUE.into(), venue.handle())]).unwrap();
        let (client, mut events) = client(&gateway).await;

        client.handle().send(order("C1")).unwrap();
        let rejected = report(&mut events).await;
        assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
        assert_eq!(rejected.text.as_deref(), Some(VENUE_UNAVAILABLE));
    }
}
