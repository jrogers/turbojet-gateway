//! The gateway end to end: a client (a Turbojet initiator) and a venue (a Turbojet acceptor with a
//! scripted application) in the test process, and Jetbridge between them, run from its
//! configuration files, over TCP on loopback.
//!
//! The venue records what it receives, and the test answers for it. Each test waits on events
//! with a timeout rather than sleeping. Runtimes are single-threaded, so [`Service::start`] tells
//! the gateway its venues before the venue's initiator can try to log on (see `VenueApp`).

use std::sync::Arc;
use std::time::Duration;

use jetbridge::service::Service;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::timeout;
use turbojet::fields::{Decimal, UtcTimestamp};
use turbojet::{
    Acceptor, Application, Context, Disconnect, FixMessage, Initiator, InitiatorConfig, MemoryStorage, Message,
    MessageReject, ReconnectPolicy, SessionConfig, SessionHandle,
};
use turbojet_fix42 as v42;
use turbojet_fix44 as v44;

const CLIENT: &str = "FUNDA";
/// The venue's `[initiator.NAME]`, and its CompID.
const VENUE: &str = "VENUE";
const GATEWAY: &str = "JETBRIDGE";
/// Orders for it are rejected by a route.
const REFUSED_SYMBOL: &str = "MSFT";
const REFUSED_TEXT: &str = "MSFT is not traded here";
/// How long any one event may take.
const WAIT: Duration = Duration::from_secs(2);

/// Something a client or venue session saw.
#[derive(Debug)]
enum Event {
    LoggedOn,
    LoggedOut,
    Message(Message),
}

/// A client's or venue's application: reports what its session sees.
struct Recorder(UnboundedSender<Event>);

impl Application for Recorder {
    fn on_logon(&self, _session: &SessionHandle) {
        let _ = self.0.send(Event::LoggedOn);
    }

    fn on_logout(&self, _session: &SessionHandle, _ended: Disconnect) {
        let _ = self.0.send(Event::LoggedOut);
    }

    fn on_message(&self, _ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let _ = self.0.send(Event::Message(msg.clone()));
        Ok(())
    }
}

fn recorder() -> (Arc<Recorder>, Events) {
    let (events, rx) = unbounded_channel();
    (Arc::new(Recorder(events)), Events(rx))
}

struct Events(UnboundedReceiver<Event>);

impl Events {
    async fn next(&mut self) -> Event {
        timeout(WAIT, self.0.recv()).await.expect("timed out").expect("closed")
    }

    async fn logged_on(&mut self) {
        let event = self.next().await;
        assert!(matches!(event, Event::LoggedOn), "expected a logon, got {event:?}");
    }

    async fn message(&mut self) -> Message {
        match self.next().await {
            Event::Message(msg) => msg,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    async fn parse<T: FixMessage>(&mut self) -> T {
        let msg = self.message().await;
        msg.parse().unwrap_or_else(|e| panic!("{e:?}: {msg:?}"))
    }
}

/// The venue: a FIX 4.4 acceptor that records what the gateway sends, and sends what the test
/// gives it.
struct Venue {
    acceptor: Acceptor,
    events: Events,
}

impl Venue {
    fn send(&self, msg: Message) {
        self.acceptor.session(GATEWAY).send(msg).expect("the gateway is logged on");
    }
}

/// The gateway's two files, its clients speaking `client_version` and its venue
/// `venue_version` at `venue_port`.
fn write_config(dir: &tempfile::TempDir, client_version: &str, venue_version: &str, venue_port: u16) {
    let sessions = format!(
        r#"
[acceptor]
begin_string = "{client_version}"
sender_comp_id = "{GATEWAY}"
listen = "127.0.0.1:0"

[counterparty.{CLIENT}]

[initiator.{VENUE}]
begin_string = "{venue_version}"
target_comp_id = "{VENUE}"
connect = ["127.0.0.1:{venue_port}"]
reconnect = {{ initial = "50ms", max = "50ms", jitter = false }}
"#
    );
    let routes = format!(
        r#"
sessions = "sessions.toml"

[[route]]
symbol = "{REFUSED_SYMBOL}"
reject = "{REFUSED_TEXT}"

[[route]]
venue = "{VENUE}"
"#
    );
    std::fs::write(dir.path().join("sessions.toml"), sessions).unwrap();
    std::fs::write(dir.path().join("jetbridge.toml"), routes).unwrap();
}

/// Jetbridge, its venue, and a client logged on through it.
struct Setup {
    service: Service,
    venue: Venue,
    client: Initiator,
    /// The client's `run`.
    running: tokio::task::JoinHandle<()>,
    events: Events,
    _dir: tempfile::TempDir,
}

impl Setup {
    /// A FIX 4.4 client and venue, both logged on.
    async fn new() -> Self {
        Self::with_versions("FIX.4.4", "FIX.4.4").await
    }

    async fn with_versions(client_version: &str, venue_version: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        write_config(&dir, client_version, venue_version, listener.local_addr().unwrap().port());
        let (app, mut venue_events) = recorder();
        let acceptor =
            Acceptor::new(SessionConfig::new(venue_version, VENUE), Arc::new(MemoryStorage::new()), app).unwrap();
        tokio::spawn(acceptor.clone().serve(listener));
        let service = start(&dir).await;
        venue_events.logged_on().await;
        let (client, mut events) = client(&service, client_version);
        let running = tokio::spawn(client.clone().run());
        events.logged_on().await;
        Self { service, venue: Venue { acceptor, events: venue_events }, client, running, events, _dir: dir }
    }

    fn send(&self, msg: Message) {
        self.client.handle().send(msg).expect("the client is logged on");
    }

    /// Sends an order for 100 at 10, and returns it as the venue got it.
    async fn order(&mut self, cl_ord_id: &str) -> v44::NewOrderSingle {
        self.send(order(cl_ord_id, "AAPL"));
        self.venue.events.parse().await
    }
}

async fn start(dir: &tempfile::TempDir) -> Service {
    let (service, _serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
    service
}

/// A client of the gateway's, not yet running.
fn client(service: &Service, version: &str) -> (Initiator, Events) {
    let (app, events) = recorder();
    let mut config = InitiatorConfig::new(SessionConfig::new(version, CLIENT), GATEWAY);
    config.reconnect = ReconnectPolicy::fixed(Duration::from_millis(50));
    let addr = service.local_addr().to_string();
    (Initiator::new(addr, config, Arc::new(MemoryStorage::new()), app).unwrap(), events)
}

fn order(cl_ord_id: &str, symbol: &str) -> Message {
    let mut order = v44::NewOrderSingle::new(cl_ord_id, v44::Side::Buy, UtcTimestamp::now(), v44::OrdType::Limit);
    (order.symbol, order.order_qty, order.price) = (Some(symbol.into()), Some(100.into()), Some(10.into()));
    order.to_message()
}

fn cancel(cl_ord_id: &str, orig_cl_ord_id: &str) -> Message {
    let mut cancel = v44::OrderCancelRequest::new(orig_cl_ord_id, cl_ord_id, v44::Side::Buy, UtcTimestamp::now());
    cancel.symbol = Some("AAPL".into());
    cancel.to_message()
}

/// A venue's report on downstream ClOrdID `cl_ord_id`, its average price 10.
fn report(
    cl_ord_id: &str,
    exec_id: &str,
    (exec_type, status): (v44::ExecType, v44::OrdStatus),
    (leaves_qty, cum_qty): (i64, i64),
) -> v44::ExecutionReport {
    let (leaves_qty, cum_qty) = (Decimal::from(leaves_qty), Decimal::from(cum_qty));
    let avg_px = if cum_qty.is_zero() { Decimal::ZERO } else { Decimal::from(10) };
    let mut report =
        v44::ExecutionReport::new("V-1", exec_id, exec_type, status, v44::Side::Buy, leaves_qty, cum_qty, avg_px);
    report.cl_ord_id = Some(cl_ord_id.into());
    report
}

fn acked(cl_ord_id: &str) -> Message {
    report(cl_ord_id, "E-new", (v44::ExecType::New, v44::OrdStatus::New), (100, 0)).to_message()
}

/// A fill of `last_qty` at 10 bringing the order of 100 to `cum_qty`.
fn fill(cl_ord_id: &str, exec_id: &str, last_qty: i64, cum_qty: i64) -> Message {
    let status = if cum_qty == 100 { v44::OrdStatus::Filled } else { v44::OrdStatus::PartiallyFilled };
    let mut fill = report(cl_ord_id, exec_id, (v44::ExecType::Trade, status), (100 - cum_qty, cum_qty));
    (fill.last_qty, fill.last_px) = (Some(last_qty.into()), Some(10.into()));
    fill.to_message()
}

/// The client's ClOrdID, our OrderID, ExecType, and LastQty, CumQty and LeavesQty of a report.
type Seen = (Option<String>, String, v44::ExecType, Option<Decimal>, Decimal, Decimal);

fn seen(report: &v44::ExecutionReport) -> Seen {
    let cl_ord_id = report.cl_ord_id.as_deref().map(str::to_owned);
    (cl_ord_id, report.order_id.to_string(), report.exec_type, report.last_qty, report.cum_qty, report.leaves_qty)
}

fn c1(exec_type: v44::ExecType, last_qty: Option<i64>, cum_qty: i64) -> Seen {
    let last_qty = last_qty.map(Decimal::from);
    (Some("C1".into()), "1".into(), exec_type, last_qty, cum_qty.into(), (100 - cum_qty).into())
}

#[tokio::test]
async fn an_order_goes_to_the_venue_and_its_ack_and_fills_come_back() {
    let mut setup = Setup::new().await;
    let sent = setup.order("C1").await;
    assert_eq!(sent.cl_ord_id, "1");
    assert_eq!((sent.order_qty, sent.price, sent.symbol.as_deref()), (Some(100.into()), Some(10.into()), Some("AAPL")));

    setup.venue.send(acked("1"));
    setup.venue.send(fill("1", "E-1", 40, 40));
    setup.venue.send(fill("1", "E-2", 60, 100));
    let new: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&new), c1(v44::ExecType::New, None, 0));
    assert_eq!(new.ord_status, v44::OrdStatus::New);
    let partial: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&partial), c1(v44::ExecType::Trade, Some(40), 40));
    assert_eq!((partial.ord_status, partial.last_px), (v44::OrdStatus::PartiallyFilled, Some(10.into())));
    let filled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&filled), c1(v44::ExecType::Trade, Some(60), 100));
    assert_eq!(filled.ord_status, v44::OrdStatus::Filled);
}

#[tokio::test]
async fn a_cancel_goes_to_the_venue_and_its_canceled_comes_back() {
    let mut setup = Setup::new().await;
    setup.order("C1").await;
    setup.venue.send(acked("1"));
    setup.events.parse::<v44::ExecutionReport>().await;

    setup.send(cancel("C2", "C1"));
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), ("1-1", "1"));
    let mut canceled = report("1-1", "E-cxl", (v44::ExecType::Canceled, v44::OrdStatus::Canceled), (0, 0));
    canceled.orig_cl_ord_id = Some("1".into());
    setup.venue.send(canceled.to_message());

    let canceled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((canceled.exec_type, canceled.ord_status), (v44::ExecType::Canceled, v44::OrdStatus::Canceled));
    assert_eq!((canceled.cl_ord_id.as_deref(), canceled.orig_cl_ord_id.as_deref()), (Some("C2"), Some("C1")));
    assert_eq!(canceled.order_id, "1");
}

#[tokio::test]
async fn a_replace_goes_to_the_venue_and_a_later_cancel_names_the_replacement() {
    let mut setup = Setup::new().await;
    setup.order("C1").await;
    setup.venue.send(acked("1"));
    setup.events.parse::<v44::ExecutionReport>().await;

    let mut replace =
        v44::OrderCancelReplaceRequest::new("C1", "C2", v44::Side::Buy, UtcTimestamp::now(), v44::OrdType::Limit);
    (replace.symbol, replace.order_qty, replace.price) = (Some("AAPL".into()), Some(150.into()), Some(11.into()));
    setup.send(replace.to_message());
    let sent: v44::OrderCancelReplaceRequest = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), ("1-1", "1"));
    assert_eq!((sent.order_qty, sent.price), (Some(150.into()), Some(11.into())));
    let mut replaced = report("1-1", "E-rpl", (v44::ExecType::Replaced, v44::OrdStatus::New), (150, 0));
    (replaced.orig_cl_ord_id, replaced.order_qty, replaced.price) =
        (Some("1".into()), Some(150.into()), Some(11.into()));
    setup.venue.send(replaced.to_message());

    let replaced: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(replaced.exec_type, v44::ExecType::Replaced);
    assert_eq!((replaced.cl_ord_id.as_deref(), replaced.orig_cl_ord_id.as_deref()), (Some("C2"), Some("C1")));
    assert_eq!(
        (replaced.order_qty, replaced.price, replaced.leaves_qty),
        (Some(150.into()), Some(11.into()), 150.into())
    );

    setup.send(cancel("C3", "C2"));
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), ("1-2", "1-1"));
}

#[tokio::test]
async fn an_order_for_a_venue_that_isnt_up_is_rejected() {
    // A port nothing listens on: bound, then let go.
    let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    write_config(&dir, "FIX.4.4", "FIX.4.4", port);
    let service = start(&dir).await;
    let (client, mut events) = client(&service, "FIX.4.4");
    tokio::spawn(client.clone().run());
    events.logged_on().await;

    client.handle().send(order("C1", "AAPL")).unwrap();
    let rejected: v44::ExecutionReport = events.parse().await;
    assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
    assert_eq!(rejected.text.as_deref(), Some("venue unavailable"));
}

#[tokio::test]
async fn a_duplicate_clordid_is_rejected() {
    let mut setup = Setup::new().await;
    setup.order("C1").await;
    setup.send(order("C1", "AAPL"));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
    assert_eq!(rejected.ord_rej_reason, Some(v44::OrdRejReason::DuplicateOrder));
}

/// A FIX 4.2 client, a FIX 4.4 venue: the order and its fill are converted both ways.
#[tokio::test]
async fn a_fix42_client_trades_on_a_fix44_venue() {
    let mut setup = Setup::with_versions("FIX.4.2", "FIX.4.4").await;
    let mut order = v42::NewOrderSingle::new(
        "C1",
        v42::HandlInst::AutomatedExecutionNoIntervention,
        "AAPL",
        v42::Side::Buy,
        UtcTimestamp::now(),
        v42::OrdType::Limit,
    );
    (order.order_qty, order.price) = (Some(100.into()), Some(10.into()));
    setup.send(order.to_message());
    let sent: v44::NewOrderSingle = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.order_qty), ("1", Some(100.into())));

    setup.venue.send(acked("1"));
    setup.venue.send(fill("1", "E-1", 100, 100));
    let new: v42::ExecutionReport = setup.events.parse().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.order_id.as_str()), (Some("C1"), "1"));
    assert_eq!((new.exec_trans_type, new.exec_type), (v42::ExecTransType::New, v42::ExecType::New));
    let filled: v42::ExecutionReport = setup.events.parse().await;
    assert_eq!((filled.exec_type, filled.ord_status), (v42::ExecType::Fill, v42::OrdStatus::Filled));
    assert_eq!((filled.last_shares, filled.cum_qty, filled.leaves_qty), (Some(100.into()), 100.into(), 0.into()));
}

#[tokio::test]
async fn a_fill_for_a_disconnected_client_arrives_once_it_logs_back_on() {
    let mut setup = Setup::new().await;
    setup.order("C1").await;
    setup.venue.send(acked("1"));
    setup.events.parse::<v44::ExecutionReport>().await;

    // The client's connection drops, and it stays away until told.
    setup.running.abort();
    assert!(setup.running.await.unwrap_err().is_cancelled());
    assert!(matches!(setup.events.next().await, Event::LoggedOut));
    setup.venue.send(fill("1", "E-1", 100, 100));
    // The book has taken the fill when the order's done; its report waits for the client.
    let gateway = setup.service.gateway().clone();
    until(|| gateway.unfinished().0.is_empty()).await;

    // The same session, its sequence numbers kept.
    tokio::spawn(setup.client.clone().run());
    setup.events.logged_on().await;
    let filled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&filled), c1(v44::ExecType::Trade, Some(100), 100));
}

#[tokio::test]
async fn a_venues_cancel_reject_reaches_the_client() {
    let mut setup = Setup::new().await;
    setup.order("C1").await;
    setup.venue.send(acked("1"));
    setup.events.parse::<v44::ExecutionReport>().await;

    setup.send(cancel("C2", "C1"));
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    let mut refused = v44::OrderCancelReject::new(
        "V-1",
        sent.cl_ord_id.as_str(),
        sent.orig_cl_ord_id.as_str(),
        v44::OrdStatus::New,
        v44::CxlRejResponseTo::OrderCancelRequest,
    );
    refused.text = Some("too late".into());
    setup.venue.send(refused.to_message());

    let refused: v44::OrderCancelReject = setup.events.parse().await;
    assert_eq!((refused.cl_ord_id.as_str(), refused.orig_cl_ord_id.as_str()), ("C2", "C1"));
    assert_eq!((refused.order_id.as_str(), refused.ord_status), ("1", v44::OrdStatus::New));
    assert_eq!(refused.cxl_rej_response_to, v44::CxlRejResponseTo::OrderCancelRequest);
}

#[tokio::test]
async fn an_order_a_route_rejects_is_rejected_with_its_text() {
    let mut setup = Setup::new().await;
    setup.send(order("C1", REFUSED_SYMBOL));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
    assert_eq!(rejected.text.as_deref(), Some(REFUSED_TEXT));
}

/// Waits for `done`, for up to [`WAIT`].
async fn until(done: impl Fn() -> bool) {
    let wait = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    timeout(WAIT, wait).await.expect("timed out waiting");
}
