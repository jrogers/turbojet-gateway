//! The gateway end to end: a client (a Turbojet initiator) and a venue (a Turbojet acceptor with a
//! scripted application) in the test process, and Jetbridge between them, run from its
//! configuration files, over TCP on loopback.
//!
//! The venue records what it receives, and the test answers for it (or, for the burst, it
//! answers every order itself). Each test waits on events with a timeout rather than sleeping.
//!
//! The venue's first logon must be one the gateway takes: [`Service::start`] tells the gateway its
//! venues right after starting their initiators, with no await between, so on the single-threaded
//! runtime most tests use, no logon can come first; on a multi-threaded one, a logon round trip
//! beating a few map inserts isn't a race that's seen.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jetbridge::book::{ClientId, Scope};
use jetbridge::service::Service;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use turbojet::fields::{Decimal, UtcTimestamp};
use turbojet::message::tags;
use turbojet::{
    Acceptor, Application, Context, Disconnect, FixMessage, Initiator, InitiatorConfig, MemoryStorage, Message,
    MessageReject, MsgType, ReconnectPolicy, SessionConfig, SessionHandle, SessionId,
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
/// The client's cancel-on-disconnect grace, where a test waits it out.
const GRACE: Duration = Duration::from_millis(200);
/// A grace no test waits out: it must not end on a loaded host before the test is done.
const LONG_GRACE: Duration = Duration::from_secs(2);

/// Something a client or venue session saw.
#[derive(Debug)]
enum Event {
    LoggedOn,
    LoggedOut,
    Message(Message),
}

/// A client's or venue's application: reports what its session sees. A venue that `answers`
/// acks each order at once, then fills 40 and 60.
struct Recorder {
    events: UnboundedSender<Event>,
    answers: bool,
}

impl Application for Recorder {
    fn on_logon(&self, _session: &SessionHandle) {
        let _ = self.events.send(Event::LoggedOn);
    }

    fn on_logout(&self, _session: &SessionHandle, _ended: Disconnect) {
        let _ = self.events.send(Event::LoggedOut);
    }

    fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let _ = self.events.send(Event::Message(msg.clone()));
        if self.answers && msg.msg_type() == MsgType::NewOrderSingle {
            let order: v44::NewOrderSingle = msg.parse()?;
            let id = order.cl_ord_id.as_str();
            for answer in [acked(id), fill(id, &format!("{id}-1"), 40, 40), fill(id, &format!("{id}-2"), 60, 100)] {
                ctx.send(answer);
            }
        }
        Ok(())
    }
}

fn recorder(answers: bool) -> (Arc<Recorder>, Events) {
    let (events, rx) = unbounded_channel();
    (Arc::new(Recorder { events, answers }), Events(rx))
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

    async fn logged_out(&mut self) {
        let event = self.next().await;
        assert!(matches!(event, Event::LoggedOut), "expected a logout, got {event:?}");
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
    serving: JoinHandle<std::io::Result<()>>,
    events: Events,
}

impl Venue {
    fn send(&self, msg: Message) {
        self.acceptor.session(GATEWAY).send(msg).expect("the gateway is logged on");
    }

    /// Sends `msg`, returning its MsgSeqNum once the venue's session has stored it.
    async fn send_stored(&self, msg: Message) -> u64 {
        let receipt = self.acceptor.session(GATEWAY).send(msg).expect("the gateway is logged on");
        timeout(WAIT, receipt).await.expect("timed out").expect("stored")
    }
}

/// A TCP relay between the gateway and the venue that can lose what the venue sends, and cut
/// every connection through it.
struct Relay {
    port: u16,
    lose: Arc<AtomicBool>,
    pumps: Arc<Mutex<JoinSet<()>>>,
}

impl Relay {
    async fn start(venue: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (lose, pumps) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(JoinSet::new())));
        let (losing, pumping) = (lose.clone(), pumps.clone());
        tokio::spawn(async move {
            while let Ok((gateway, _)) = listener.accept().await {
                let Ok(venue) = TcpStream::connect(venue).await else { continue };
                let ((from_gateway, to_gateway), (from_venue, to_venue)) = (gateway.into_split(), venue.into_split());
                let mut pumps = pumping.lock().unwrap();
                pumps.spawn(pump(from_gateway, to_venue, None));
                pumps.spawn(pump(from_venue, to_gateway, Some(losing.clone())));
            }
        });
        Self { port, lose, pumps }
    }

    /// From now on, what the venue sends is lost until [`cut`](Self::cut).
    fn lose(&self) {
        self.lose.store(true, Ordering::Release);
    }

    /// Closes every connection through the relay; later ones pass everything.
    fn cut(&self) {
        self.pumps.lock().unwrap().abort_all();
        self.lose.store(false, Ordering::Release);
    }
}

/// Copies `from` to `to`, dropping what arrives while `lose` is set.
async fn pump(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, lose: Option<Arc<AtomicBool>>) {
    let mut buf = vec![0; 4096];
    while let Ok(n @ 1..) = from.read(&mut buf).await {
        if lose.as_ref().is_some_and(|lose| lose.load(Ordering::Acquire)) {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

/// The routes: orders for `symbol` rejected with `text`, the rest to the venue.
fn routes(symbol: &str, text: &str) -> String {
    format!(
        "sessions = \"sessions.toml\"\n[[route]]\nsymbol = \"{symbol}\"\nreject = \"{text}\"\n[[route]]\nvenue = \"{VENUE}\"\n"
    )
}

/// The gateway's two files, its clients speaking `client_version` and its venue
/// `venue_version` at `venue_port`.
fn write_config(dir: &tempfile::TempDir, options: &Options, venue_port: u16) {
    let Options { client_version, venue_version, cancel_on_disconnect, .. } = options;
    let cancel = cancel_on_disconnect
        .map(|(trigger, grace)| {
            format!("cancel_on_disconnect = \"{trigger}\"\ncancel_grace = \"{}ms\"\n", grace.as_millis())
        })
        .unwrap_or_default();
    let sessions = format!(
        r#"
[acceptor]
begin_string = "{client_version}"
sender_comp_id = "{GATEWAY}"
listen = "127.0.0.1:0"

[counterparty.{CLIENT}]
{cancel}
[initiator.{VENUE}]
begin_string = "{venue_version}"
target_comp_id = "{VENUE}"
connect = ["127.0.0.1:{venue_port}"]
reconnect = {{ initial = "50ms", max = "50ms", jitter = false }}
"#
    );
    std::fs::write(dir.path().join("sessions.toml"), sessions).unwrap();
    std::fs::write(dir.path().join("jetbridge.toml"), routes(REFUSED_SYMBOL, REFUSED_TEXT)).unwrap();
}

struct Options {
    client_version: &'static str,
    venue_version: &'static str,
    /// The gateway reaches the venue through a [`Relay`].
    relay: bool,
    /// The venue answers every order itself.
    answers: bool,
    /// The client's `cancel_on_disconnect` setting and its grace.
    cancel_on_disconnect: Option<(&'static str, Duration)>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            client_version: "FIX.4.4",
            venue_version: "FIX.4.4",
            relay: false,
            answers: false,
            cancel_on_disconnect: None,
        }
    }
}

/// Jetbridge, its venue, and a client logged on through it.
struct Setup {
    service: Service,
    venue: Venue,
    relay: Option<Relay>,
    client: Initiator,
    /// The client's `run`.
    running: JoinHandle<()>,
    events: Events,
    dir: tempfile::TempDir,
    /// What every ID the gateway issues starts with.
    prefix: String,
}

impl Setup {
    /// A FIX 4.4 client and venue, both logged on.
    async fn new() -> Self {
        Self::with(Options::default()).await
    }

    async fn with(options: Options) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let venue_addr = listener.local_addr().unwrap();
        let relay = if options.relay { Some(Relay::start(venue_addr).await) } else { None };
        let dir = tempfile::tempdir().unwrap();
        write_config(&dir, &options, relay.as_ref().map_or(venue_addr.port(), |relay| relay.port));
        let (app, mut venue_events) = recorder(options.answers);
        let config = SessionConfig::new(options.venue_version, VENUE);
        let acceptor = Acceptor::new(config, Arc::new(MemoryStorage::new()), app).unwrap();
        let serving = tokio::spawn(acceptor.clone().serve(listener));
        let (service, _serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        venue_events.logged_on().await;
        let (client, mut events) = client(&service, options.client_version, None);
        let running = tokio::spawn(client.clone().run());
        events.logged_on().await;
        let venue = Venue { acceptor, serving, events: venue_events };
        let prefix = service.gateway().id_prefix().to_string();
        Self { service, venue, relay, client, running, events, dir, prefix }
    }

    /// The gateway's ID `id`, as it issues it: after its prefix.
    fn id(&self, id: &str) -> String {
        format!("{}{id}", self.prefix)
    }

    /// What the client should see of order C1 of 100, our order 1.
    fn c1(&self, exec_id: &str, exec_type: v44::ExecType, last_qty: Option<i64>, cum_qty: i64) -> Seen {
        let last_qty = last_qty.map(Decimal::from);
        let (cum, leaves) = (cum_qty.into(), (100 - cum_qty).into());
        (Some("C1".into()), self.id("1"), self.id(exec_id), exec_type, last_qty, cum, leaves)
    }

    fn send(&self, msg: Message) {
        self.client.handle().send(msg).expect("the client is logged on");
    }

    /// Sends an order for 100 AAPL at 10, and returns it as the venue got it.
    async fn order(&mut self, cl_ord_id: &str) -> v44::NewOrderSingle {
        self.send(order(cl_ord_id, "AAPL"));
        self.venue.events.parse().await
    }

    /// Order C1, acked by the venue, the client having seen the ack.
    async fn working_order(&mut self) {
        self.order("C1").await;
        self.venue.send(acked(&self.id("1")));
        let new: v44::ExecutionReport = self.events.parse().await;
        assert_eq!(seen(&new), self.c1("1.1", v44::ExecType::New, None, 0));
    }

    fn venue_session(&self) -> SessionHandle {
        self.service.venue_session(VENUE).expect("the venue's initiator runs")
    }
}

/// A client of the gateway's, not yet running, with SenderSubID `sub_id` if given.
fn client(service: &Service, version: &str, sub_id: Option<&str>) -> (Initiator, Events) {
    let (app, events) = recorder(false);
    let mut config = InitiatorConfig::new(SessionConfig::new(version, CLIENT), GATEWAY);
    config.sender_sub_id = sub_id.map(Into::into);
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
    report(cl_ord_id, &format!("{cl_ord_id}-new"), (v44::ExecType::New, v44::OrdStatus::New), (100, 0)).to_message()
}

/// A fill of `last_qty` at 10 bringing the order of 100 to `cum_qty`.
fn fill(cl_ord_id: &str, exec_id: &str, last_qty: i64, cum_qty: i64) -> Message {
    let status = if cum_qty == 100 { v44::OrdStatus::Filled } else { v44::OrdStatus::PartiallyFilled };
    let mut fill = report(cl_ord_id, exec_id, (v44::ExecType::Trade, status), (100 - cum_qty, cum_qty));
    (fill.last_qty, fill.last_px) = (Some(last_qty.into()), Some(10.into()));
    fill.to_message()
}

/// Of a client's report: ClOrdID, OrderID, ExecID, ExecType, LastQty, CumQty and LeavesQty.
type Seen = (Option<String>, String, String, v44::ExecType, Option<Decimal>, Decimal, Decimal);

fn seen(report: &v44::ExecutionReport) -> Seen {
    let cl_ord_id = report.cl_ord_id.as_deref().map(str::to_owned);
    let (order_id, exec_id) = (report.order_id.to_string(), report.exec_id.to_string());
    (cl_ord_id, order_id, exec_id, report.exec_type, report.last_qty, report.cum_qty, report.leaves_qty)
}

/// A Rejected report for `cl_ord_id` the book made without booking an order.
fn assert_unbooked_reject(rejected: &v44::ExecutionReport, cl_ord_id: &str, exec_id: &str) {
    assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some(cl_ord_id), v44::ExecType::Rejected));
    assert_eq!((rejected.order_id.as_str(), rejected.exec_id.as_str()), ("NONE", exec_id));
    assert_eq!(rejected.ord_status, v44::OrdStatus::Rejected);
}

#[tokio::test]
async fn an_order_goes_to_the_venue_and_its_ack_and_fills_come_back() {
    let mut setup = Setup::new().await;
    let sent = setup.order("C1").await;
    assert_eq!(sent.cl_ord_id, &*setup.id("1"));
    assert_eq!(setup.prefix.len(), 6, "the gateway prefixes its IDs with its start");
    assert_eq!((sent.order_qty, sent.price, sent.symbol.as_deref()), (Some(100.into()), Some(10.into()), Some("AAPL")));

    setup.venue.send(acked(&setup.id("1")));
    setup.venue.send(fill(&setup.id("1"), "E-1", 40, 40));
    setup.venue.send(fill(&setup.id("1"), "E-2", 60, 100));
    let new: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&new), setup.c1("1.1", v44::ExecType::New, None, 0));
    assert_eq!(new.ord_status, v44::OrdStatus::New);
    let partial: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&partial), setup.c1("1.2", v44::ExecType::Trade, Some(40), 40));
    assert_eq!((partial.ord_status, partial.last_px), (v44::OrdStatus::PartiallyFilled, Some(10.into())));
    let filled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&filled), setup.c1("1.3", v44::ExecType::Trade, Some(60), 100));
    assert_eq!(filled.ord_status, v44::OrdStatus::Filled);
}

#[tokio::test]
async fn a_cancel_goes_to_the_venue_and_its_canceled_comes_back() {
    let mut setup = Setup::new().await;
    setup.working_order().await;

    setup.send(cancel("C2", "C1"));
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), (&*setup.id("1-1"), &*setup.id("1")));
    let mut canceled = report(&setup.id("1-1"), "E-cxl", (v44::ExecType::Canceled, v44::OrdStatus::Canceled), (0, 0));
    canceled.orig_cl_ord_id = Some(setup.id("1").into());
    setup.venue.send(canceled.to_message());

    let canceled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((canceled.exec_type, canceled.ord_status), (v44::ExecType::Canceled, v44::OrdStatus::Canceled));
    assert_eq!((canceled.cl_ord_id.as_deref(), canceled.orig_cl_ord_id.as_deref()), (Some("C2"), Some("C1")));
    assert_eq!((canceled.order_id.as_str(), canceled.exec_id.as_str()), (&*setup.id("1"), &*setup.id("1.2")));
    assert_eq!((canceled.cum_qty, canceled.leaves_qty), (0.into(), 0.into()));
}

#[tokio::test]
async fn a_replace_goes_to_the_venue_and_a_later_cancel_names_the_replacement() {
    let mut setup = Setup::new().await;
    setup.working_order().await;

    let mut replace =
        v44::OrderCancelReplaceRequest::new("C1", "C2", v44::Side::Buy, UtcTimestamp::now(), v44::OrdType::Limit);
    (replace.symbol, replace.order_qty, replace.price) = (Some("AAPL".into()), Some(150.into()), Some(11.into()));
    setup.send(replace.to_message());
    let sent: v44::OrderCancelReplaceRequest = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), (&*setup.id("1-1"), &*setup.id("1")));
    assert_eq!((sent.order_qty, sent.price), (Some(150.into()), Some(11.into())));
    let mut replaced = report(&setup.id("1-1"), "E-rpl", (v44::ExecType::Replaced, v44::OrdStatus::New), (150, 0));
    (replaced.orig_cl_ord_id, replaced.order_qty, replaced.price) =
        (Some(setup.id("1").into()), Some(150.into()), Some(11.into()));
    setup.venue.send(replaced.to_message());

    let replaced: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(replaced.exec_type, v44::ExecType::Replaced);
    assert_eq!((replaced.cl_ord_id.as_deref(), replaced.orig_cl_ord_id.as_deref()), (Some("C2"), Some("C1")));
    assert_eq!((replaced.order_id.as_str(), replaced.exec_id.as_str()), (&*setup.id("1"), &*setup.id("1.2")));
    assert_eq!((replaced.order_qty, replaced.price), (Some(150.into()), Some(11.into())));
    assert_eq!((replaced.cum_qty, replaced.leaves_qty), (0.into(), 150.into()));

    setup.send(cancel("C3", "C2"));
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), (&*setup.id("1-2"), &*setup.id("1-1")));
}

#[tokio::test]
async fn an_order_for_a_venue_that_went_down_is_rejected() {
    let mut setup = Setup::new().await;
    setup.venue.acceptor.shutdown(Some("closing")).await;
    // Its listener closed too, so no reconnect gets as far as a logon.
    let serving = &mut setup.venue.serving;
    timeout(WAIT, serving).await.expect("still serving").unwrap().unwrap();
    setup.venue.events.logged_out().await;
    let venue = setup.venue_session();
    until(|| !venue.is_connected()).await;

    setup.send(order("C1", "AAPL"));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
    assert_eq!((rejected.order_id.as_str(), rejected.exec_id.as_str()), (&*setup.id("1"), &*setup.id("1.1")));
    assert_eq!(rejected.ord_status, v44::OrdStatus::Rejected);
    assert_eq!(rejected.text.as_deref(), Some("venue unavailable"));
}

/// The duplicate never reaches the venue: the next order is the venue's next message.
#[tokio::test]
async fn a_duplicate_clordid_is_rejected() {
    let mut setup = Setup::new().await;
    setup.order("C1").await;
    setup.send(order("C1", "AAPL"));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_unbooked_reject(&rejected, "C1", &setup.id("R1"));
    assert_eq!(rejected.ord_rej_reason, Some(v44::OrdRejReason::DuplicateOrder));
    assert_eq!(setup.order("C2").await.cl_ord_id, &*setup.id("2"));
}

/// A FIX 4.2 client, a FIX 4.4 venue: the order and its fill are converted both ways.
#[tokio::test]
async fn a_fix42_client_trades_on_a_fix44_venue() {
    let mut setup = Setup::with(Options { client_version: "FIX.4.2", ..Options::default() }).await;
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
    assert_eq!((sent.cl_ord_id.as_str(), sent.order_qty), (&*setup.id("1"), Some(100.into())));

    setup.venue.send(acked(&setup.id("1")));
    setup.venue.send(fill(&setup.id("1"), "E-1", 100, 100));
    let new: v42::ExecutionReport = setup.events.parse().await;
    assert_eq!(
        (new.cl_ord_id.as_deref(), new.order_id.as_str(), new.exec_id.as_str()),
        (Some("C1"), &*setup.id("1"), &*setup.id("1.1"))
    );
    assert_eq!((new.exec_trans_type, new.exec_type), (v42::ExecTransType::New, v42::ExecType::New));
    let filled: v42::ExecutionReport = setup.events.parse().await;
    assert_eq!((filled.exec_trans_type, filled.exec_type), (v42::ExecTransType::New, v42::ExecType::Fill));
    assert_eq!((filled.ord_status, filled.exec_id.as_str()), (v42::OrdStatus::Filled, &*setup.id("1.2")));
    assert_eq!((filled.last_shares, filled.cum_qty, filled.leaves_qty), (Some(100.into()), 100.into(), 0.into()));
}

/// The fill is held in the outbox while the client is away, and sent afresh, not resent, once
/// it's back.
#[tokio::test]
async fn a_fill_for_a_disconnected_client_arrives_once_it_logs_back_on() {
    let mut setup = Setup::new().await;
    setup.working_order().await;

    // The client's connection drops, and it stays away until told.
    setup.running.abort();
    assert!((&mut setup.running).await.unwrap_err().is_cancelled());
    setup.events.logged_out().await;
    let client = setup.service.client_session(&SessionId::new("FIX.4.4", GATEWAY, CLIENT));
    until(|| !client.is_connected()).await;
    setup.venue.send(fill(&setup.id("1"), "E-1", 100, 100));
    // The book has taken the fill when the order's done.
    let gateway = setup.service.gateway().clone();
    until(|| gateway.unfinished().open_orders.is_empty()).await;
    assert_eq!(gateway.unfinished().undelivered, BTreeMap::from([(ClientId::new(client.id()), 1)]));

    // The same session, its sequence numbers kept.
    tokio::spawn(setup.client.clone().run());
    setup.events.logged_on().await;
    let msg = setup.events.message().await;
    assert_eq!(msg.get(tags::POSS_DUP_FLAG), None);
    assert_eq!(seen(&msg.parse().unwrap()), setup.c1("1.2", v44::ExecType::Trade, Some(100), 100));
    until(|| gateway.unfinished().undelivered.is_empty()).await;
}

#[tokio::test]
async fn a_venues_cancel_reject_reaches_the_client() {
    let mut setup = Setup::new().await;
    setup.working_order().await;

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
    assert_eq!((refused.order_id.as_str(), refused.ord_status), (&*setup.id("1"), v44::OrdStatus::New));
    assert_eq!(refused.cxl_rej_response_to, v44::CxlRejResponseTo::OrderCancelRequest);
}

/// The rejected order never reaches the venue: the next order is the venue's next message.
#[tokio::test]
async fn an_order_a_route_rejects_is_rejected_with_its_text() {
    let mut setup = Setup::new().await;
    setup.send(order("C1", REFUSED_SYMBOL));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_unbooked_reject(&rejected, "C1", &setup.id("R1"));
    assert_eq!(rejected.text.as_deref(), Some(REFUSED_TEXT));
    assert_eq!(setup.order("C2").await.cl_ord_id, &*setup.id("1"));
}

/// A reload's routes apply to new orders; a working order's reports still reach its client.
#[tokio::test]
async fn reloaded_routes_apply_to_new_orders_and_working_ones_carry_on() {
    let mut setup = Setup::new().await;
    setup.working_order().await;
    std::fs::write(setup.dir.path().join("jetbridge.toml"), routes("AAPL", "AAPL is halted")).unwrap();
    setup.service.reload();

    setup.send(order("C2", "AAPL"));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_unbooked_reject(&rejected, "C2", &setup.id("R1"));
    assert_eq!(rejected.text.as_deref(), Some("AAPL is halted"));
    setup.venue.send(fill(&setup.id("1"), "E-1", 100, 100));
    let filled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&filled), setup.c1("1.2", v44::ExecType::Trade, Some(100), 100));
}

/// The venue's connection drops with a fill it stored but the gateway never got, which the venue
/// resends after the reconnect; then the gateway asks for both fills again. The client sees each
/// fill once.
#[tokio::test]
async fn venue_resends_after_a_reconnect_reach_the_client_once() {
    let mut setup = Setup::with(Options { relay: true, ..Options::default() }).await;
    let relay = setup.relay.take().expect("a relay");
    setup.working_order().await;
    let first = setup.venue.send_stored(fill(&setup.id("1"), "E-1", 40, 40)).await;
    let partial: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(seen(&partial), setup.c1("1.2", v44::ExecType::Trade, Some(40), 40));

    relay.lose();
    let last = setup.venue.send_stored(fill(&setup.id("1"), "E-2", 60, 100)).await;
    relay.cut();
    // Stored at the venue, but lost on the way.
    assert_eq!(setup.service.gateway().unfinished().open_orders, BTreeMap::from([(VENUE.into(), 1)]));
    setup.venue.events.logged_out().await;
    setup.venue.events.logged_on().await;
    // The gateway saw the venue's Logon run ahead, and asked for the fill it missed.
    let msg = setup.events.message().await;
    assert_eq!(seen(&msg.parse().unwrap()), setup.c1("1.3", v44::ExecType::Trade, Some(60), 100));

    let venue = setup.venue_session();
    assert_eq!(venue.request_resend(first).await.unwrap().next_incoming, first);
    let caught_up = async {
        while venue.sequence_numbers().await.unwrap().next_incoming <= last {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    timeout(WAIT, caught_up).await.expect("the resends never arrived");
    // Nothing for order 1 comes before the next order's ack.
    assert_eq!(setup.order("C2").await.cl_ord_id, &*setup.id("2"));
    setup.venue.send(acked(&setup.id("2")));
    let new: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C2"), v44::ExecType::New));
}

/// Two sessions of one CompID, told apart by SubID, are two clients: each may use the same
/// ClOrdID, and each gets only its own orders' reports while the other (and the CompID's session
/// without a SubID) is connected too.
#[tokio::test]
async fn sessions_of_one_comp_id_each_get_their_own_reports() {
    let mut setup = Setup::new().await;
    let mut desks = Vec::new();
    for sub_id in ["DESK1", "DESK2"] {
        let (desk, mut events) = client(&setup.service, "FIX.4.4", Some(sub_id));
        tokio::spawn(desk.clone().run());
        events.logged_on().await;
        desks.push((desk, events));
    }
    for (desk, _) in &desks {
        desk.handle().send(order("C1", "AAPL")).expect("the desk is logged on");
        let sent: v44::NewOrderSingle = setup.venue.events.parse().await;
        assert!(sent.cl_ord_id.starts_with(&setup.prefix), "{sent:?}");
    }

    // Acked the other way round, so a report sent to the wrong desk would come first.
    setup.venue.send(acked(&setup.id("2")));
    setup.venue.send(acked(&setup.id("1")));
    for (n, (_, events)) in desks.iter_mut().enumerate() {
        let new: v44::ExecutionReport = events.parse().await;
        let order_id = setup.id(&(n + 1).to_string());
        assert_eq!((new.cl_ord_id.as_deref(), new.order_id.as_str()), (Some("C1"), &*order_id));
    }
    setup.venue.send(fill(&setup.id("2"), "E-1", 100, 100));
    let filled: v44::ExecutionReport = desks[1].1.parse().await;
    assert_eq!((filled.order_id.as_str(), filled.exec_type), (&*setup.id("2"), v44::ExecType::Trade));
    let gateway = setup.service.gateway().clone();
    until(|| gateway.unfinished().undelivered.is_empty()).await;
    assert!(desks[0].1.0.try_recv().is_err() && setup.events.0.try_recv().is_err());
}

/// The venue's Canceled for the gateway's cancel `downstream` of order `orig`.
fn canceled(downstream: &str, orig: &str) -> Message {
    let mut canceled =
        report(downstream, &format!("{downstream}-cxl"), (v44::ExecType::Canceled, v44::OrdStatus::Canceled), (0, 0));
    canceled.orig_cl_ord_id = Some(orig.into());
    canceled.to_message()
}

/// The kill switch on the client's session: the venue gets a cancel for each working order, the
/// client hears of each end unasked, and its new orders are refused until it's unblocked.
#[tokio::test]
async fn the_kill_switch_cancels_a_sessions_orders_and_blocks_it() {
    let mut setup = Setup::new().await;
    setup.working_order().await;
    setup.order("C2").await;
    setup.venue.send(acked(&setup.id("2")));
    let new: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!(new.cl_ord_id.as_deref(), Some("C2"));

    let gateway = setup.service.gateway().clone();
    let session = Scope::Session(ClientId::new(&SessionId::new("FIX.4.4", GATEWAY, CLIENT)));
    gateway.cancel_all(session.clone());
    assert_eq!(gateway.blocked(), std::slice::from_ref(&session));
    for n in ["1", "2"] {
        let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
        assert_eq!(
            (sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()),
            (&*setup.id(&format!("{n}-1")), &*setup.id(n))
        );
        setup.venue.send(canceled(&sent.cl_ord_id, &sent.orig_cl_ord_id));
    }
    for cl_ord_id in ["C1", "C2"] {
        let canceled: v44::ExecutionReport = setup.events.parse().await;
        assert_eq!((canceled.cl_ord_id.as_deref(), canceled.orig_cl_ord_id.as_deref()), (Some(cl_ord_id), None));
        assert_eq!((canceled.exec_type, canceled.ord_status), (v44::ExecType::Canceled, v44::OrdStatus::Canceled));
    }
    assert!(gateway.cancelling().is_empty());

    setup.send(order("C3", "AAPL"));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_unbooked_reject(&rejected, "C3", &setup.id("R1"));
    assert_eq!(rejected.text.as_deref(), Some("blocked by an operator"));
    assert_eq!(rejected.ord_rej_reason, Some(v44::OrdRejReason::Other));
    gateway.unblock(session);
    assert_eq!(setup.order("C4").await.cl_ord_id, &*setup.id("3"));
}

/// A block of the CompID covers a session of it that logs on afterwards.
#[tokio::test]
async fn a_comp_id_block_covers_a_session_that_logs_on_later() {
    let setup = Setup::new().await;
    setup.service.gateway().block(Scope::CompId(CLIENT.into()));
    let (desk, mut events) = client(&setup.service, "FIX.4.4", Some("DESK1"));
    tokio::spawn(desk.clone().run());
    events.logged_on().await;
    desk.handle().send(order("C1", "AAPL")).expect("the desk is logged on");
    let rejected: v44::ExecutionReport = events.parse().await;
    assert_unbooked_reject(&rejected, "C1", &setup.id("R1"));
    assert_eq!(rejected.text.as_deref(), Some("blocked by an operator"));
}

/// A client with cancel on disconnect after `grace` and a working order, its connection dropped
/// without a Logout and the gateway having seen it go; and when it dropped.
async fn dropped_with_a_working_order(grace: Duration) -> (Setup, Instant) {
    let mut setup =
        Setup::with(Options { cancel_on_disconnect: Some(("disconnect", grace)), ..Options::default() }).await;
    setup.working_order().await;
    let dropped = Instant::now();
    setup.running.abort();
    assert!((&mut setup.running).await.unwrap_err().is_cancelled());
    setup.events.logged_out().await;
    let client = setup.service.client_session(&SessionId::new("FIX.4.4", GATEWAY, CLIENT));
    until(|| !client.is_connected()).await;
    (setup, dropped)
}

/// After the grace, the gateway cancels the order of a client that dropped; the client isn't
/// blocked, and hears of the cancel once it logs back on.
#[tokio::test]
async fn a_client_that_drops_has_its_orders_cancelled_after_the_grace() {
    let (mut setup, dropped) = dropped_with_a_working_order(GRACE).await;
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    assert!(dropped.elapsed() >= GRACE, "cancelled within the grace");
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), (&*setup.id("1-1"), &*setup.id("1")));
    setup.venue.send(canceled(&sent.cl_ord_id, &sent.orig_cl_ord_id));
    let gateway = setup.service.gateway().clone();
    until(|| gateway.unfinished().open_orders.is_empty()).await;
    assert!(gateway.blocked().is_empty() && gateway.cancelling().is_empty());

    tokio::spawn(setup.client.clone().run());
    setup.events.logged_on().await;
    let canceled: v44::ExecutionReport = setup.events.parse().await;
    assert_eq!((canceled.cl_ord_id.as_deref(), canceled.orig_cl_ord_id.as_deref()), (Some("C1"), None));
    assert_eq!((canceled.exec_type, canceled.ord_status), (v44::ExecType::Canceled, v44::OrdStatus::Canceled));
    assert_eq!(setup.order("C2").await.cl_ord_id, &*setup.id("2"));
}

/// A client back within the grace keeps its orders.
#[tokio::test]
async fn a_client_back_within_the_grace_keeps_its_orders() {
    let (mut setup, _) = dropped_with_a_working_order(LONG_GRACE).await;
    tokio::spawn(setup.client.clone().run());
    setup.events.logged_on().await;
    tokio::time::sleep(LONG_GRACE + GRACE).await;
    assert!(setup.venue.events.0.try_recv().is_err(), "the venue got a cancel");
    assert_eq!(setup.service.gateway().unfinished().open_orders.len(), 1);
}

/// Under `disconnect`, a Logout isn't a disconnect: the orders stay.
#[tokio::test]
async fn a_client_that_logs_out_keeps_its_orders() {
    let mut setup =
        Setup::with(Options { cancel_on_disconnect: Some(("disconnect", GRACE)), ..Options::default() }).await;
    setup.working_order().await;
    setup.client.shutdown(None).await;
    setup.events.logged_out().await;
    tokio::time::sleep(2 * GRACE).await;
    assert!(setup.venue.events.0.try_recv().is_err(), "the venue got a cancel");
    assert_eq!(setup.service.gateway().unfinished().open_orders.len(), 1);
}

/// Under `disconnect_or_logout`, it is.
#[tokio::test]
async fn a_client_that_logs_out_under_disconnect_or_logout_has_its_orders_cancelled() {
    let mut setup =
        Setup::with(Options { cancel_on_disconnect: Some(("disconnect_or_logout", GRACE)), ..Options::default() })
            .await;
    setup.working_order().await;
    setup.client.shutdown(None).await;
    setup.events.logged_out().await;
    let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
    assert_eq!(sent.orig_cl_ord_id.as_str(), &*setup.id("1"));
}

/// Shutdown stops the acceptor first, which fires the countdowns under way, so their cancels
/// reach the venue before it's logged out; and it waits for the venue's answers.
#[tokio::test]
async fn shutdown_sends_pending_cancels_before_logging_the_venues_out() {
    let (mut setup, _) = dropped_with_a_working_order(LONG_GRACE).await;
    let shutdown = setup.service.shutdown();
    let answering = async {
        let sent: v44::OrderCancelRequest = setup.venue.events.parse().await;
        setup.venue.send(canceled(&sent.cl_ord_id, &sent.orig_cl_ord_id));
        setup.venue.events.logged_out().await;
    };
    timeout(WAIT, async { tokio::join!(shutdown, answering) }).await.expect("shutdown hung");
    let unfinished = setup.service.gateway().unfinished();
    assert!(unfinished.open_orders.is_empty() && unfinished.cancelling.is_empty(), "{unfinished:?}");
    // The client is gone: its Canceled report is the one thing left.
    assert_eq!(unfinished.undelivered.values().sum::<usize>(), 1);
}

#[tokio::test]
async fn shutting_down_logs_the_client_and_the_venue_out() {
    let mut setup = Setup::new().await;
    timeout(WAIT, setup.service.shutdown()).await.expect("shutdown hung");
    setup.events.logged_out().await;
    setup.venue.events.logged_out().await;
}

/// Many orders at once, each acked and filled twice by the venue as it arrives: every client
/// sees each of its orders' reports in order, numbered by the book.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_orders_each_reports_in_order() {
    const ORDERS: usize = 200;
    let mut setup = Setup::with(Options { answers: true, ..Options::default() }).await;
    for i in 0..ORDERS {
        setup.send(order(&format!("C{i}"), "AAPL"));
    }
    let mut by_order: HashMap<String, Vec<v44::ExecutionReport>> = HashMap::new();
    for _ in 0..3 * ORDERS {
        let report: v44::ExecutionReport = setup.events.parse().await;
        by_order.entry(report.cl_ord_id.clone().expect("a ClOrdID").to_string()).or_default().push(report);
    }
    assert_eq!(by_order.len(), ORDERS);
    let mut order_ids = HashSet::new();
    for reports in by_order.values() {
        let id = reports[0].order_id.to_string();
        let got: Vec<_> = reports
            .iter()
            .map(|r| (r.order_id.to_string(), r.exec_id.to_string(), r.exec_type, r.cum_qty, r.leaves_qty))
            .collect();
        let expected: Vec<_> =
            [(1, v44::ExecType::New, 0), (2, v44::ExecType::Trade, 40), (3, v44::ExecType::Trade, 100)]
                .into_iter()
                .map(|(n, exec_type, cum)| (id.clone(), format!("{id}.{n}"), exec_type, cum.into(), (100 - cum).into()))
                .collect();
        assert_eq!(got, expected);
        assert!(order_ids.insert(id));
    }
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

/// Limits from the reloaded file apply: an order over max_order_qty is rejected with the limit's
/// text and Order exceeds limit, and never reaches the venue; one within it does.
#[tokio::test]
async fn an_order_over_max_order_qty_is_rejected_with_the_limits_text() {
    let mut setup = Setup::new().await;
    let risk = "[risk.defaults]\nmax_order_qty = 99\n";
    std::fs::write(setup.dir.path().join("jetbridge.toml"), routes(REFUSED_SYMBOL, REFUSED_TEXT) + risk).unwrap();
    setup.service.reload();

    setup.send(order("C1", "AAPL"));
    let rejected: v44::ExecutionReport = setup.events.parse().await;
    assert_unbooked_reject(&rejected, "C1", &setup.id("R1"));
    assert_eq!(rejected.text.as_deref(), Some("quantity over the limit (risk max_order_qty)"));
    assert_eq!(rejected.ord_rej_reason, Some(v44::OrdRejReason::OrderExceedsLimit));
    std::fs::write(setup.dir.path().join("jetbridge.toml"), routes(REFUSED_SYMBOL, REFUSED_TEXT)).unwrap();
    setup.service.reload();
    assert_eq!(setup.order("C2").await.cl_ord_id, &*setup.id("1"));
}
