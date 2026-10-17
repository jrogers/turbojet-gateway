//! The crash suite: Turbojet Gateway with disk session stores and a SQLite file, stopped dead at each step
//! of an order's flow and started again on the same files, with a client and a venue that stay up
//! and reconnect, as in production. Each test checks what the client and the venue saw end to
//! end, and that the database, once quiet, agrees.
//!
//! **The crash.** The gateway runs on a single-threaded tokio runtime on a thread of its own. A
//! crash ends the runtime's `block_on` and shuts it down without polling anything again: every
//! task is dropped where it last yielded, so no shutdown path runs, nothing is flushed, no Logout
//! goes, and no write starts. The service itself is leaked, not dropped. What the tasks own is
//! dropped with them, unpolled: the writer forgets the holds of what it hadn't committed, as it
//! does whenever its runtime is torn down, and the sockets, session logs (with a disk store's
//! lock) and the writer's database connection (with a transaction it had open, uncommitted)
//! close, as at a dying process's exit. Two things can finish after the crash, as they can when
//! a process dies: a disk store's write and fsync already running on a blocking thread (the crash
//! waits for it, so its lock is released before the restart), and a SQLite statement already
//! handed to sqlx's worker thread. An OS crash could lose what they write (and unsynced writes
//! that a process crash keeps): these tests are of process crashes. Dropping a connected session
//! runs Turbojet's `Session::drop`, which calls `on_logout` and starts cancel on disconnect's
//! countdown; nothing it starts is ever polled, so neither has a lasting effect.
//!
//! **Where it crashes.** Gates, not sleeps: the venue's and the client's session stores at the
//! gateway hold their commits while their gates are shut (so they neither store nor send), and
//! the database's writer waits before each write, or inside its transaction after its statements,
//! while the store's test gates are shut.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;
use turbojet::fields::{Code, Decimal, UtcTimestamp};
use turbojet::message::tags;
use turbojet::store::Fetched;
use turbojet::{
    Acceptor, Application, Context, Disconnect, DiskStorage, FixMessage, Initiator, InitiatorConfig, MemoryStorage,
    Message, MessageReject, MsgType, ReconnectPolicy, SessionConfig, SessionHandle, SessionStorage,
};
use turbojet_fix::fix44 as v44;

use crate::book::{self, Snapshot, Status};
use crate::edge::tests::Gated;
use crate::service::{Service, StartOptions};
use crate::store::Store;

const CLIENT: &str = "FUNDA";
const VENUE: &str = "VENUE";
const GATEWAY: &str = "TURBOJETGATEWAY";
/// How long any one event may take: a restart waits up to a second for its ID prefix.
const WAIT: Duration = Duration::from_secs(5);
/// How long a test waits to see that nothing more arrives: loopback delivers in well under it.
const QUIET: Duration = Duration::from_millis(100);

/// Something the client or the venue saw.
#[derive(Debug)]
enum Event {
    LoggedOn,
    LoggedOut,
    Message(Message),
}

/// Records what it sees; asked for messages again, it resends them only if `resends`, else
/// gap-fills them.
struct Recorder {
    events: UnboundedSender<Event>,
    resends: bool,
}

impl Application for Recorder {
    fn on_logon(&self, _: &SessionHandle) {
        let _ = self.events.send(Event::LoggedOn);
    }

    fn on_logout(&self, _: &SessionHandle, _: Disconnect) {
        let _ = self.events.send(Event::LoggedOut);
    }

    fn on_message(&self, _: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let _ = self.events.send(Event::Message(msg.clone()));
        Ok(())
    }

    fn should_resend(&self, _: &SessionHandle, _: &Message) -> bool {
        self.resends
    }
}

fn recorder(resends: bool) -> (Arc<Recorder>, Events) {
    let (events, rx) = unbounded_channel();
    (Arc::new(Recorder { events, resends }), Events(rx))
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

    /// The next logout, past status reports (see [`report`](Self::report)).
    async fn logged_out(&mut self) {
        loop {
            match self.next().await {
                Event::LoggedOut => return,
                Event::Message(msg) if msg.get(tags::EXEC_TYPE) == Some("I") => {}
                event => panic!("expected a logout, got {event:?}"),
            }
        }
    }

    async fn message(&mut self) -> Message {
        match self.next().await {
            Event::Message(msg) => msg,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// The next message but status reports: those answer a client's request redelivered after a
    /// restart (Turbojet redelivers what its last commit hadn't marked done), and say where the
    /// order stands, changing nothing.
    async fn report(&mut self) -> Message {
        loop {
            let msg = self.message().await;
            if msg.get(tags::EXEC_TYPE) != Some("I") {
                return msg;
            }
        }
    }

    async fn parse<T: FixMessage>(&mut self) -> T {
        let msg = self.message().await;
        msg.parse().unwrap_or_else(|e| panic!("{e:?}: {msg:?}"))
    }

    async fn execution_report(&mut self) -> v44::ExecutionReport {
        let msg = self.report().await;
        msg.parse().unwrap_or_else(|e| panic!("{e:?}: {msg:?}"))
    }

    /// Nothing more has arrived.
    fn assert_empty(&mut self) {
        if let Ok(event) = self.0.try_recv() {
            panic!("unexpected {event:?}");
        }
    }

    /// Nothing but status reports arrives for [`QUIET`].
    async fn assert_quiet(&mut self) {
        let quiet = async {
            while let Some(event) = self.0.recv().await {
                let status = matches!(&event, Event::Message(msg) if msg.get(tags::EXEC_TYPE) == Some("I"));
                assert!(status, "unexpected {event:?}");
            }
        };
        assert!(timeout(QUIET, quiet).await.is_err(), "closed");
    }
}

/// Turbojet Gateway on a runtime and thread of its own, so it can be stopped dead: see the module
/// documentation.
struct Gateway {
    service: Arc<Service>,
    runtime: tokio::runtime::Handle,
    stop: oneshot::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Gateway {
    /// Starts Turbojet Gateway from `dir`'s files, serving clients on `listener`, its venue's and
    /// client's sessions in gated disk stores, open while `venue` and `client` are.
    fn start(
        dir: &Path,
        listener: std::net::TcpListener,
        venue: watch::Receiver<bool>,
        client: watch::Receiver<bool>,
    ) -> Self {
        let path = dir.join("turbojet-gateway.toml");
        let stores = vec![
            ("venue".to_string(), gated_disk(&dir.join("venue-store"), venue)),
            ("client".to_string(), gated_disk(&dir.join("client-store"), client)),
        ];
        // A loaded machine (several suites at once) can take over the 1 s the writer allows a
        // batch before it rejects new orders as "gateway unavailable": the designed back-pressure,
        // which these tests aren't about.
        let options =
            StartOptions { stores, listener: Some(listener), api_listener: None, max_commit_wait: Some(WAIT) };
        let (started, starting) = std::sync::mpsc::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let (service, serving) = runtime.block_on(Service::start_with(&path, options)).unwrap();
            started.send((service.clone(), runtime.handle().clone())).unwrap();
            let _ = runtime.block_on(stopped);
            // The crash: see the module documentation. A blocking job not yet started never runs.
            std::mem::forget((service, serving));
            runtime.shutdown_timeout(WAIT);
        });
        let (service, runtime) = starting.recv_timeout(WAIT).expect("the gateway didn't start");
        Self { service, runtime, stop, thread }
    }

    /// Runs `future` on the gateway's runtime: whatever it spawns dies with the gateway.
    async fn run<T: Send + 'static>(&self, future: impl Future<Output = T> + Send + 'static) -> T {
        timeout(WAIT, self.runtime.spawn(future)).await.expect("timed out").expect("panicked")
    }

    /// Waits until everything the book has changed so far has committed.
    async fn committed(&self) {
        let gateway = self.service.gateway().clone();
        self.run(async move { gateway.committed().await }).await;
    }

    fn store(&self) -> Store {
        self.service.store().expect("the gateway has a database").clone()
    }

    /// Stops it dead.
    fn crash(self) {
        let Self { service, stop, thread, .. } = self;
        stop.send(()).expect("the gateway is running");
        thread.join().expect("the gateway's thread panicked");
        std::mem::forget(service);
    }

    /// Shuts it down, then stops it.
    async fn shut_down(self) {
        let service = self.service.clone();
        self.run(async move { service.shutdown().await }).await;
        self.crash();
    }
}

/// The client, the venue and the gateway between them, logged on.
struct Setup {
    dir: tempfile::TempDir,
    /// The clients' listener, a copy given to each start.
    listener: std::net::TcpListener,
    gateway: Option<Gateway>,
    /// Shut, the venue's session at the gateway stores nothing.
    venue_gate: watch::Sender<bool>,
    /// Shut, the client's session at the gateway stores nothing.
    client_gate: watch::Sender<bool>,
    venue: Acceptor,
    venue_events: Events,
    client: Initiator,
    /// The client's `run`, while it's connected or reconnecting.
    running: Option<tokio::task::JoinHandle<()>>,
    events: Events,
    /// Each start's ID prefix, oldest first.
    prefixes: Vec<String>,
}

impl Setup {
    async fn new() -> Self {
        Self::with(Options::default()).await
    }

    async fn with(options: Options) -> Self {
        // RUST_LOG=info, say, to see what the gateway logs.
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter(EnvFilter::from_default_env()).try_init();
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let venue_port = listener.local_addr().unwrap().port();
        let (app, mut venue_events) = recorder(true);
        let venue = Acceptor::new(SessionConfig::new("FIX.4.4", VENUE), Arc::new(MemoryStorage::new()), app).unwrap();
        tokio::spawn(venue.clone().serve(listener));
        // Kept open across starts, so the port stays the gateway's while it's down: a client
        // connecting then waits in the backlog for the next start, as under socket activation.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        write_config(dir.path(), port, venue_port, &options);
        let (venue_gate, venue_open) = watch::channel(true);
        let (client_gate, client_open) = watch::channel(true);
        let gateway = Gateway::start(dir.path(), listener.try_clone().unwrap(), venue_open, client_open);
        venue_events.logged_on().await;

        let (app, mut events) = recorder(options.client_resends);
        let mut config = InitiatorConfig::new(SessionConfig::new("FIX.4.4", CLIENT), GATEWAY);
        config.reconnect = ReconnectPolicy::fixed(Duration::from_millis(20));
        let client = Initiator::new(format!("127.0.0.1:{port}"), config, Arc::new(MemoryStorage::new()), app).unwrap();
        let running = Some(tokio::spawn(client.clone().run()));
        events.logged_on().await;
        let prefixes = vec![gateway.service.gateway().id_prefix().to_string()];
        let gateway = Some(gateway);
        Self { dir, listener, gateway, venue_gate, client_gate, venue, venue_events, client, running, events, prefixes }
    }

    fn gateway(&self) -> &Gateway {
        self.gateway.as_ref().expect("the gateway is running")
    }

    /// The ID `id` of start `start` (0 the first).
    fn id(&self, start: usize, id: &str) -> String {
        format!("{}{id}", self.prefixes[start])
    }

    fn send(&self, msg: Message) {
        self.client.handle().send(msg).expect("the client is logged on");
    }

    fn venue_send(&self, msg: Message) {
        self.venue.session(GATEWAY).send(msg).expect("the gateway is logged on to the venue");
    }

    /// Sends `msg` from the venue, once its session has stored it.
    async fn venue_send_stored(&self, msg: Message) {
        let receipt = self.venue.session(GATEWAY).send(msg).expect("the gateway is logged on to the venue");
        timeout(WAIT, receipt).await.expect("timed out").expect("stored");
    }

    /// Order `cl_ord_id`, booked as the start's order `n`, acked by the venue, its ack seen.
    async fn working_order(&mut self, cl_ord_id: &str, n: usize) -> String {
        self.send(order(cl_ord_id));
        let sent: v44::NewOrderSingle = self.venue_events.parse().await;
        let downstream = self.id(self.prefixes.len() - 1, &n.to_string());
        assert_eq!(sent.cl_ord_id.as_str(), downstream);
        self.venue_send(acked(&downstream, 100));
        let new = self.events.execution_report().await;
        assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some(cl_ord_id), Code::Known(v44::ExecType::New)));
        downstream
    }

    /// Nothing more is on its way to the client or the venue: a last order's request is the
    /// venue's next message and its ack the client's (status reports aside), and nothing else
    /// arrives for a while after.
    async fn assert_quiet(&mut self) {
        self.send(order("LAST"));
        let sent: v44::NewOrderSingle = self.venue_events.parse().await;
        let last = self.id(self.prefixes.len() - 1, "");
        assert!(sent.cl_ord_id.starts_with(&last), "{sent:?}");
        self.venue_send(acked(&sent.cl_ord_id, 100));
        let new = self.events.execution_report().await;
        assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("LAST"), Code::Known(v44::ExecType::New)));
        tokio::join!(self.events.assert_quiet(), self.venue_events.assert_quiet());
    }

    /// Stops the gateway dead, and starts it again on its files once the client and venue have
    /// seen it go.
    async fn crash_and_restart(&mut self) {
        self.gateway.take().expect("the gateway is running").crash();
        self.restart().await;
    }

    /// Starts the gateway again once the client (if connected) and venue have seen it go,
    /// waiting for both to log on.
    async fn restart(&mut self) {
        self.gone().await;
        self.start_again().await;
    }

    /// Waits for the client (if connected) and the venue to see the gateway go.
    async fn gone(&mut self) {
        if self.running.is_some() {
            self.events.logged_out().await;
        }
        self.venue_events.logged_out().await;
    }

    /// Starts the gateway again, waiting for the venue and the client (if running) to log on.
    async fn start_again(&mut self) {
        let (venue_gate, venue_open) = watch::channel(true);
        let (client_gate, client_open) = watch::channel(true);
        (self.venue_gate, self.client_gate) = (venue_gate, client_gate);
        let gateway = Gateway::start(self.dir.path(), self.listener.try_clone().unwrap(), venue_open, client_open);
        self.prefixes.push(gateway.service.gateway().id_prefix().to_string());
        self.gateway = Some(gateway);
        self.venue_events.logged_on().await;
        if self.running.is_some() {
            self.events.logged_on().await;
        }
    }

    /// Resets the venue's session at the gateway, as a schedule would, and the venue's side of
    /// it, while the gateway is down: its store starts empty, created now.
    async fn reset_venue_session(&self) {
        let disk = DiskStorage::new(self.dir.path().join("venue-store"), true).unwrap();
        let mut log = disk.open(&turbojet::SessionId::new("FIX.4.4", GATEWAY, VENUE)).unwrap();
        log.reset().unwrap();
        log.set_created_at(UtcTimestamp::now()).unwrap();
        if let Some(commit) = log.commit().unwrap() {
            commit.run().unwrap();
        }
        drop(log);
        self.venue.session(GATEWAY).reset_sequence_numbers().await.unwrap();
    }

    /// While the gateway is down: how many messages the venue's session at the gateway has
    /// numbered, and how many of them from `begin` a fetch returns, as a scan from there reads
    /// (a fetch skips what the store keeps no copy of, a Logon or a heartbeat).
    fn venue_store_read(&self, begin: u64) -> (u64, usize) {
        let disk = DiskStorage::new(self.dir.path().join("venue-store"), true).unwrap();
        let mut log = disk.open(&turbojet::SessionId::new("FIX.4.4", GATEWAY, VENUE)).unwrap();
        let holds = log.next_outgoing() - 1;
        if begin > holds {
            return (holds, 0);
        }
        let fetched = match log.fetch(begin, holds).unwrap() {
            Fetched::Ready(fetched) => fetched,
            Fetched::Pending(job) => job.run().unwrap(),
        };
        (holds, fetched.len())
    }

    /// Takes a checkpoint of the venue's store once what the book changed has committed, and the
    /// gateway has seen the venue's logon (which may trail the venue's), and waits for it to
    /// commit too.
    async fn checkpoint(&self) -> book::Checkpoint {
        self.gateway().committed().await;
        let gateway = self.gateway().service.gateway().clone();
        let taken = async {
            loop {
                let taken = gateway.checkpoint_once();
                if !taken.is_empty() {
                    return taken;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let taken = timeout(WAIT, taken).await.expect("no checkpoint taken");
        assert_eq!(taken.len(), 1, "{taken:?}");
        self.gateway().committed().await;
        taken[0].1
    }

    /// Drops the client's connection, without a Logout, and waits for the gateway to see it go.
    async fn disconnect_client(&mut self) {
        let running = self.running.take().expect("the client is running");
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        self.events.logged_out().await;
        let id = turbojet::SessionId::new("FIX.4.4", GATEWAY, CLIENT);
        let session = self.gateway().service.client_session(&id);
        until(|| !session.is_connected()).await;
    }

    /// The client logs back on, its sequence numbers kept.
    async fn reconnect_client(&mut self) {
        assert!(self.running.is_none(), "the client is running");
        self.running = Some(tokio::spawn(self.client.clone().run()));
        self.events.logged_on().await;
    }

    /// Waits until the database satisfies `done`.
    async fn until_stored(&self, done: impl Fn(&Snapshot) -> bool) {
        let wait = async {
            while !done(&self.database().await) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(WAIT, wait).await.expect("timed out waiting for the database");
    }

    /// The database, once everything the book changed has committed.
    async fn database(&self) -> Snapshot {
        self.gateway().committed().await;
        let store = self.gateway().store();
        self.gateway().run(async move { store.load().await }).await.unwrap()
    }
}

/// A session store on disk in `dir`, committing only while `open`.
fn gated_disk(dir: &Path, open: watch::Receiver<bool>) -> Arc<dyn SessionStorage> {
    let disk = DiskStorage::new(dir, true).unwrap();
    Arc::new(Gated { inner: Arc::new(disk), open })
}

fn database_url(dir: &Path) -> String {
    format!("sqlite://{}?mode=rwc", dir.join("turbojet-gateway.db").display())
}

/// How a [`Setup`] differs from the usual.
struct Options {
    /// The client is cancelled on disconnect after this.
    cancel_on_disconnect: Option<Duration>,
    /// The book is in a SQLite file.
    database: bool,
    /// The venue's session resets at each logon.
    reset_on_logon: bool,
    /// Asked for messages again, the client resends them; else it gap-fills them.
    client_resends: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { cancel_on_disconnect: None, database: true, reset_on_logon: false, client_resends: true }
    }
}

/// Turbojet Gateway's files: clients on `port`, the venue at `venue_port`, both sessions on disk,
/// as `options` say.
fn write_config(dir: &Path, port: u16, venue_port: u16, options: &Options) {
    let Options { cancel_on_disconnect, database, reset_on_logon, .. } = options;
    let cancel = cancel_on_disconnect
        .map(|grace| format!("cancel_on_disconnect = \"disconnect\"\ncancel_grace = \"{}ms\"\n", grace.as_millis()))
        .unwrap_or_default();
    let sessions = format!(
        r#"
[acceptor]
begin_string = "FIX.4.4"
sender_comp_id = "{GATEWAY}"
listen = "127.0.0.1:{port}"

[store.disk]
kind = "disk"
dir = "store"

[defaults]
store = "disk"

[counterparty.{CLIENT}]
store = "client"
{cancel}
[initiator.{VENUE}]
begin_string = "FIX.4.4"
target_comp_id = "{VENUE}"
connect = ["127.0.0.1:{venue_port}"]
reconnect = {{ initial = "20ms", max = "20ms", jitter = false }}
store = "venue"
reset_on_logon = {reset_on_logon}
"#
    );
    std::fs::write(dir.join("sessions.toml"), sessions).unwrap();
    let database = if *database { format!("[database]\nurl = \"{}\"\n", database_url(dir)) } else { String::new() };
    let config = format!("sessions = \"sessions.toml\"\n[[route]]\nvenue = \"{VENUE}\"\n{database}");
    std::fs::write(dir.join("turbojet-gateway.toml"), config).unwrap();
}

fn order(cl_ord_id: &str) -> Message {
    let mut order = v44::NewOrderSingle::new(cl_ord_id, v44::Side::Buy, UtcTimestamp::now(), v44::OrdType::Limit);
    (order.symbol, order.order_qty, order.price) = (Some("AAPL".into()), Some(100.into()), Some(10.into()));
    order.to_message()
}

fn cancel(cl_ord_id: &str, orig_cl_ord_id: &str) -> Message {
    let mut cancel = v44::OrderCancelRequest::new(orig_cl_ord_id, cl_ord_id, v44::Side::Buy, UtcTimestamp::now());
    cancel.symbol = Some("AAPL".into());
    cancel.to_message()
}

fn replace(cl_ord_id: &str, orig_cl_ord_id: &str, qty: i64) -> Message {
    let mut replace = v44::OrderCancelReplaceRequest::new(
        orig_cl_ord_id,
        cl_ord_id,
        v44::Side::Buy,
        UtcTimestamp::now(),
        v44::OrdType::Limit,
    );
    (replace.symbol, replace.order_qty, replace.price) = (Some("AAPL".into()), Some(qty.into()), Some(11.into()));
    replace.to_message()
}

/// A venue's report on downstream ClOrdID `cl_ord_id`, of an order for `qty` at 10.
fn report(
    cl_ord_id: &str,
    exec_id: &str,
    (exec_type, status): (v44::ExecType, v44::OrdStatus),
    (qty, cum_qty): (i64, i64),
) -> v44::ExecutionReport {
    let (leaves_qty, cum) = (Decimal::from(qty - cum_qty), Decimal::from(cum_qty));
    let avg_px = if cum_qty == 0 { Decimal::ZERO } else { Decimal::from(10) };
    let leaves_qty = if matches!(exec_type, v44::ExecType::Canceled) { Decimal::ZERO } else { leaves_qty };
    let mut report =
        v44::ExecutionReport::new("V-1", exec_id, exec_type, status, v44::Side::Buy, leaves_qty, cum, avg_px);
    report.cl_ord_id = Some(cl_ord_id.into());
    report
}

fn acked(cl_ord_id: &str, qty: i64) -> Message {
    report(cl_ord_id, &format!("{cl_ord_id}-new"), (v44::ExecType::New, v44::OrdStatus::New), (qty, 0)).to_message()
}

/// A fill of `last_qty` bringing the order of 100 to `cum_qty`.
fn fill(cl_ord_id: &str, exec_id: &str, last_qty: i64, cum_qty: i64) -> Message {
    fill_of(cl_ord_id, exec_id, last_qty, (100, cum_qty))
}

/// A fill of `last_qty` bringing an order of `qty` to `cum_qty`.
fn fill_of(cl_ord_id: &str, exec_id: &str, last_qty: i64, (qty, cum_qty): (i64, i64)) -> Message {
    let status = if cum_qty == qty { v44::OrdStatus::Filled } else { v44::OrdStatus::PartiallyFilled };
    let mut fill = report(cl_ord_id, exec_id, (v44::ExecType::Trade, status), (qty, cum_qty));
    (fill.last_qty, fill.last_px) = (Some(last_qty.into()), Some(10.into()));
    fill.to_message()
}

/// The venue's Canceled for cancel `downstream` of order `orig`, `cum_qty` of 100 filled.
fn canceled(downstream: &str, orig: &str, cum_qty: i64) -> Message {
    let exec_id = format!("{downstream}-cxl");
    let mut canceled =
        report(downstream, &exec_id, (v44::ExecType::Canceled, v44::OrdStatus::Canceled), (100, cum_qty));
    canceled.orig_cl_ord_id = Some(orig.into());
    canceled.to_message()
}

/// The status and filled quantity of each order the database has under client ClOrdID
/// `cl_ord_id` (an order's latest).
fn stored(database: &Snapshot, cl_ord_id: &str) -> Vec<(Status, Decimal)> {
    let orders = database.orders.values().filter(|row| row.order.request.cl_ord_id == cl_ord_id);
    orders.map(|row| (row.order.status, row.order.cum_qty)).collect()
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

/// Crash after the venue's session stored (and sent) a new order, before the database recorded
/// it: at the restart the orphan is found in the venue's store and cancelled at its logon, and
/// the client's redelivered order is booked and sent afresh. The client sees one ack, for the
/// new order; the venue sees the orphan cancelled and the order again.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_an_order_commits_cancels_the_orphan_and_books_the_resent_order() {
    let mut setup = Setup::new().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.send(order("C1"));
    let sent: v44::NewOrderSingle = setup.venue_events.parse().await;
    let orphan = setup.id(0, "1");
    assert_eq!(sent.cl_ord_id.as_str(), orphan);

    setup.crash_and_restart().await;
    let (mut cancel, mut again) = (None, None);
    for _ in 0..2 {
        let msg = setup.venue_events.message().await;
        match msg.msg_type() {
            MsgType::OrderCancelRequest => cancel = Some(msg.parse::<v44::OrderCancelRequest>().unwrap()),
            MsgType::NewOrderSingle => again = Some(msg.parse::<v44::NewOrderSingle>().unwrap()),
            other => panic!("unexpected {other:?}: {msg:?}"),
        }
    }
    let (cancel, again) = (cancel.expect("the orphan's cancel"), again.expect("the order again"));
    assert_eq!(cancel.orig_cl_ord_id.as_str(), orphan);
    assert_eq!(again.cl_ord_id.as_str(), setup.id(1, "1"));
    setup.venue_send(canceled(&cancel.cl_ord_id, &orphan, 0));
    setup.venue_send(acked(&again.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C1"), Code::Known(v44::ExecType::New)));
    assert_eq!(new.order_id.as_str(), setup.id(1, "1"));

    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
    setup.assert_quiet().await;
}

/// Of a client's fill: ExecID, LastQty, CumQty and PossResend(97).
fn fill_seen(msg: &Message) -> (String, Option<Decimal>, Decimal, Option<String>) {
    let report: v44::ExecutionReport = msg.parse().unwrap();
    assert_eq!(report.exec_type, v44::ExecType::Trade, "{report:?}");
    let poss_resend = msg.get(tags::POSS_RESEND).map(str::to_owned);
    (report.exec_id.to_string(), report.last_qty, report.cum_qty, poss_resend)
}

/// Crash after a fill committed while the client was away, its report waiting in the outbox:
/// after the restart the report reaches the client once, marked PossResend.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_waiting_for_a_disconnected_client_reaches_it_after_a_crash() {
    let mut setup = Setup::new().await;
    let downstream = setup.working_order("C1", 1).await;
    setup.disconnect_client().await;
    setup.venue_send(fill(&downstream, "E-1", 100, 100));
    let gateway = setup.gateway().service.gateway().clone();
    until(|| gateway.unfinished().open_orders.is_empty()).await;
    drop(gateway);
    setup.gateway().committed().await;

    setup.crash_and_restart().await;
    setup.reconnect_client().await;
    let filled = fill_seen(&setup.events.report().await);
    assert_eq!(filled, (setup.id(0, "1.2"), Some(100.into()), 100.into(), Some("Y".into())));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Filled, 100.into())]);
    assert!(database.undelivered.is_empty(), "{:?}", database.undelivered);
}

/// Crash after a fill committed, before the client's session at the gateway stored its report
/// (its store gated), so it never went: after the restart it reaches the client once, marked
/// PossResend.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_the_clients_session_never_stored_reaches_it_after_a_crash() {
    let mut setup = Setup::new().await;
    let downstream = setup.working_order("C1", 1).await;
    setup.client_gate.send_replace(false);
    setup.venue_send(fill(&downstream, "E-1", 100, 100));
    let gateway = setup.gateway().service.gateway().clone();
    until(|| gateway.unfinished().open_orders.is_empty()).await;
    drop(gateway);
    setup.gateway().committed().await;
    setup.events.assert_empty();

    setup.crash_and_restart().await;
    let filled = fill_seen(&setup.events.report().await);
    assert_eq!(filled, (setup.id(0, "1.2"), Some(100.into()), 100.into(), Some("Y".into())));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Filled, 100.into())]);
    assert!(database.undelivered.is_empty(), "{:?}", database.undelivered);
}

/// The kill switch, then a crash with one of its cancels answered and one not: after the
/// restart the block holds, the unanswered cancel's answer still applies, and the client hears
/// of each order's end once.
#[tokio::test(flavor = "multi_thread")]
async fn a_block_and_its_cancels_carry_on_across_a_crash() {
    let mut setup = Setup::new().await;
    let first = setup.working_order("C1", 1).await;
    let second = setup.working_order("C2", 2).await;
    let client = book::ClientId::new(&turbojet::SessionId::new("FIX.4.4", GATEWAY, CLIENT));
    let scope = book::Scope::Session(client);
    let gateway = setup.gateway().service.gateway().clone();
    let cancelling = scope.clone();
    setup.gateway().run(async move { gateway.cancel_all(cancelling) }).await;
    let mut cancels = Vec::new();
    for _ in 0..2 {
        let sent: v44::OrderCancelRequest = setup.venue_events.parse().await;
        cancels.push((sent.cl_ord_id.to_string(), sent.orig_cl_ord_id.to_string()));
    }
    cancels.sort();
    assert_eq!(cancels.iter().map(|(_, orig)| orig.as_str()).collect::<Vec<_>>(), [&*first, &*second]);
    setup.venue_send(canceled(&cancels[0].0, &first, 0));
    let canceled_report = setup.events.execution_report().await;
    assert_eq!(
        (canceled_report.cl_ord_id.as_deref(), canceled_report.exec_type),
        (Some("C1"), Code::Known(v44::ExecType::Canceled))
    );
    setup.gateway().committed().await;

    setup.crash_and_restart().await;
    assert_eq!(setup.gateway().service.gateway().blocked(), std::slice::from_ref(&scope));
    setup.venue_send(canceled(&cancels[1].0, &second, 0));
    let canceled_report = setup.events.execution_report().await;
    assert_eq!(
        (canceled_report.cl_ord_id.as_deref(), canceled_report.exec_type),
        (Some("C2"), Code::Known(v44::ExecType::Canceled))
    );
    setup.send(order("C3"));
    let rejected = setup.events.execution_report().await;
    assert_eq!((rejected.cl_ord_id.as_deref(), rejected.text.as_deref()), (Some("C3"), Some("blocked by an operator")));

    let gateway = setup.gateway().service.gateway().clone();
    assert!(setup.gateway().run(async move { gateway.unblock(scope) }).await);
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Canceled, Decimal::ZERO)]);
    assert_eq!(stored(&database, "C2"), [(Status::Canceled, Decimal::ZERO)]);
    assert!(database.blocked.is_empty());
}

/// The kill switch's cancels stored by the venue's session but not committed when the gateway
/// crashes (the API would have answered the operator 202, not committed): after the restart the
/// orphaned cancels are found, and the orders are cancelled; the block is lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_switch_that_never_committed_still_cancels_after_a_crash() {
    let mut setup = Setup::new().await;
    let first = setup.working_order("C1", 1).await;
    setup.gateway().committed().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    let gateway = setup.gateway().service.gateway().clone();
    setup.gateway().runtime.spawn(async move { gateway.cancel_all(book::Scope::All) });
    let sent: v44::OrderCancelRequest = setup.venue_events.parse().await;
    assert_eq!(sent.orig_cl_ord_id.as_str(), first);

    setup.crash_and_restart().await;
    assert!(setup.gateway().service.gateway().blocked().is_empty());
    // The gateway cancels the order again at the logon, the orphan's cancel having been taken
    // as its own; the venue's answer to the orphaned one applies to the order.
    let again: v44::OrderCancelRequest = setup.venue_events.parse().await;
    assert_eq!(again.orig_cl_ord_id.as_str(), first);
    setup.venue_send(canceled(&sent.cl_ord_id, &first, 0));
    let canceled_report = setup.events.execution_report().await;
    assert_eq!(
        (canceled_report.cl_ord_id.as_deref(), canceled_report.exec_type),
        (Some("C1"), Code::Known(v44::ExecType::Canceled))
    );
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Canceled, Decimal::ZERO)]);
}

/// Client C1's order, booked as order 1, acked and filled 40, the fill seen and committed; its
/// downstream ClOrdID.
async fn partly_filled(setup: &mut Setup) -> String {
    let downstream = setup.working_order("C1", 1).await;
    setup.venue_send(fill(&downstream, "E-1", 40, 40));
    let partial = setup.events.execution_report().await;
    assert_eq!((partial.exec_type, partial.cum_qty), (Code::Known(v44::ExecType::Trade), 40.into()));
    setup.gateway().committed().await;
    downstream
}

/// The venue accepts replace `downstream` of `orig` to 150 at 11, 40 filled.
fn replaced(downstream: &str, orig: &str) -> Message {
    let exec_id = format!("{downstream}-rpl");
    let mut replaced =
        report(downstream, &exec_id, (v44::ExecType::Replaced, v44::OrdStatus::PartiallyFilled), (150, 40));
    (replaced.orig_cl_ord_id, replaced.order_qty, replaced.price) =
        (Some(orig.into()), Some(150.into()), Some(11.into()));
    replaced.to_message()
}

/// Of a client's report: ClOrdID, ExecType, OrderQty, CumQty and LeavesQty.
type Terms = (Option<String>, Code<v44::ExecType>, Option<Decimal>, Decimal, Decimal);

fn terms(report: &v44::ExecutionReport) -> Terms {
    let cl_ord_id = report.cl_ord_id.as_deref().map(str::to_owned);
    (cl_ord_id, report.exec_type.clone(), report.order_qty, report.cum_qty, report.leaves_qty)
}

/// A replace the venue's session stored and sent, not yet committed at the crash: after the
/// restart the orphaned replace is found and recorded pending, the client's redelivered replace
/// is taken as it, and the venue's Replaced answers the client. The order keeps working and the
/// fill after reaches the client.
#[tokio::test(flavor = "multi_thread")]
async fn a_replace_in_flight_at_a_crash_is_answered_and_its_order_keeps_working() {
    replace_in_flight_at_a_crash(true).await;
}

/// As [`a_replace_in_flight_at_a_crash_is_answered_and_its_order_keeps_working`], but the
/// client's session gap-fills the replace the gateway asks for again, and the client sends it
/// afresh, unmarked: it's still taken as the orphan and answered Replaced.
#[tokio::test(flavor = "multi_thread")]
async fn an_unmarked_resent_replace_is_answered_after_a_crash() {
    replace_in_flight_at_a_crash(false).await;
}

/// A replace in flight at a crash, the client's session resending it (`resends`) or the client
/// sending it again unmarked.
async fn replace_in_flight_at_a_crash(resends: bool) {
    let mut setup = Setup::with(Options { client_resends: resends, ..Options::default() }).await;
    let order = partly_filled(&mut setup).await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.send(replace("C2", "C1", 150));
    let sent: v44::OrderCancelReplaceRequest = setup.venue_events.parse().await;
    assert_eq!((sent.orig_cl_ord_id.as_str(), sent.order_qty), (&*order, Some(150.into())));

    setup.crash_and_restart().await;
    if !resends {
        setup.send(replace("C2", "C1", 150));
    }
    let downstream = sent.cl_ord_id.to_string();
    let adopted = |database: &Snapshot| database.sent.get(&*downstream).is_some_and(|sent| sent.cl_ord_id == "C2");
    setup.until_stored(adopted).await;
    setup.venue_send(replaced(&sent.cl_ord_id, &order));
    setup.venue_send(fill_of(&sent.cl_ord_id, "E-2", 20, (150, 60)));
    let replaced = setup.events.execution_report().await;
    assert_eq!(
        terms(&replaced),
        (Some("C2".into()), Code::Known(v44::ExecType::Replaced), Some(150.into()), 40.into(), 110.into())
    );
    let filled = setup.events.execution_report().await;
    assert_eq!(
        terms(&filled),
        (Some("C2".into()), Code::Known(v44::ExecType::Trade), Some(150.into()), 60.into(), 90.into())
    );
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C2"), [(Status::PartiallyFilled, 60.into())]);
}

/// Cancel on disconnect fires, its cancel stored and sent by the venue's session, and the gateway
/// crashes before committing it: after the restart the orphaned cancel is the gateway's, its
/// answer applies, and the client hears of the cancel once it's back.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_during_cancel_on_disconnect_still_cancels_and_tells_the_client() {
    let mut setup =
        Setup::with(Options { cancel_on_disconnect: Some(Duration::from_millis(50)), ..Options::default() }).await;
    let order = setup.working_order("C1", 1).await;
    setup.gateway().committed().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.disconnect_client().await;
    let sent: v44::OrderCancelRequest = setup.venue_events.parse().await;
    assert_eq!(sent.orig_cl_ord_id.as_str(), order);

    setup.crash_and_restart().await;
    let again: v44::OrderCancelRequest = setup.venue_events.parse().await;
    assert_eq!(again.orig_cl_ord_id.as_str(), order);
    setup.venue_send(canceled(&sent.cl_ord_id, &order, 0));
    let mut too_late = v44::OrderCancelReject::new(
        "V-1",
        again.cl_ord_id.as_str(),
        order.as_str(),
        v44::OrdStatus::Canceled,
        v44::CxlRejResponseTo::OrderCancelRequest,
    );
    too_late.cxl_rej_reason = Some(v44::CxlRejReason::TooLateToCancel.into());
    setup.venue_send(too_late.to_message());
    setup.reconnect_client().await;
    let canceled = setup.events.execution_report().await;
    assert_eq!((canceled.cl_ord_id.as_deref(), canceled.exec_type), (Some("C1"), Code::Known(v44::ExecType::Canceled)));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Canceled, Decimal::ZERO)]);
}

/// A clean shutdown and a restart: the order carries on, and the redelivery of the last commit
/// window (if any) changes nothing the client sees.
#[tokio::test(flavor = "multi_thread")]
async fn a_clean_shutdown_and_restart_carries_on() {
    let mut setup = Setup::new().await;
    let order = partly_filled(&mut setup).await;
    setup.gateway.take().expect("the gateway is running").shut_down().await;
    setup.restart().await;

    setup.venue_send(fill(&order, "E-2", 60, 100));
    let filled = fill_seen(&setup.events.report().await);
    assert_eq!(filled, (setup.id(0, "1.3"), Some(60.into()), 100.into(), None));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Filled, 100.into())]);
    assert!(database.undelivered.is_empty(), "{:?}", database.undelivered);
}

/// Crash after a fill reached the gateway, and its report the client, before it committed: the
/// venue redelivers the fill after the restart, and the client gets the same report again, marked
/// PossResend, so it can tell it's had it; its cumulative quantity stays right.
#[tokio::test(flavor = "multi_thread")]
async fn a_fill_lost_in_a_crash_is_redelivered_and_reported_again_as_possibly_resent() {
    let mut setup = Setup::new().await;
    let downstream = setup.working_order("C1", 1).await;
    setup.gateway().committed().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.venue_send_stored(fill(&downstream, "E-1", 40, 40)).await;
    let mut fills = vec![fill_seen(&setup.events.report().await)];

    setup.crash_and_restart().await;
    setup.venue_send(fill(&downstream, "E-2", 60, 100));
    while fills.last().is_none_or(|(.., cum, _)| *cum != Decimal::from(100)) {
        fills.push(fill_seen(&setup.events.report().await));
    }
    let partial = (setup.id(0, "1.2"), Some(40.into()), 40.into(), None);
    let again = (setup.id(0, "1.2"), Some(40.into()), 40.into(), Some("Y".into()));
    // Turbojet marks the new fill too if it arrives while the gateway's ResendRequest for the
    // first is outstanding, within the window that may have been handled: it can't tell them
    // apart then. Either way the client has a new ExecID to take.
    let flag = fills[2].3.take();
    assert!(matches!(flag.as_deref(), None | Some("Y")), "{flag:?}");
    let filled = (setup.id(0, "1.3"), Some(60.into()), 100.into(), None);
    assert_eq!(fills, [partial, again, filled]);
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Filled, 100.into())]);
}

/// Crash before the venue's session stores a new order: it never left, and the database has it
/// booked. After the restart it goes at the venue's logon, once (the client's redelivery of it
/// is answered with its status), and the client sees one ack.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_before_the_venue_stores_an_order_sends_it_once_after_the_restart() {
    let mut setup = Setup::new().await;
    setup.venue_gate.send_replace(false);
    setup.send(order("C1"));
    setup.until_stored(|database| !stored(database, "C1").is_empty()).await;
    setup.venue_events.assert_empty();

    setup.crash_and_restart().await;
    let msg = setup.venue_events.message().await;
    assert_eq!(msg.get(tags::POSS_RESEND), Some("Y"));
    let sent: v44::NewOrderSingle = msg.parse().unwrap();
    assert_eq!(sent.cl_ord_id.as_str(), setup.id(0, "1"));
    setup.venue_send(acked(&sent.cl_ord_id, 100));
    // The ack, and the status answering the client's redelivered order, in either order.
    let mut seen = Vec::new();
    for _ in 0..2 {
        let report: v44::ExecutionReport = setup.events.parse().await;
        assert_eq!((report.cl_ord_id.as_deref(), report.order_id.as_str()), (Some("C1"), &*setup.id(0, "1")));
        seen.push(report.exec_type);
    }
    seen.sort_by_key(|exec_type| *exec_type == v44::ExecType::New);
    assert_eq!(seen, [v44::ExecType::OrderStatus, v44::ExecType::New]);
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
}

/// A replace the venue's session never stored at the crash, though the database has it pending:
/// after the restart it reaches the venue once; the client hears it replaced, and of the fills
/// after.
#[tokio::test(flavor = "multi_thread")]
async fn a_replace_the_venue_never_got_before_a_crash_goes_after_it() {
    let mut setup = Setup::new().await;
    let order = partly_filled(&mut setup).await;
    setup.venue_gate.send_replace(false);
    setup.send(replace("C2", "C1", 150));
    setup.until_stored(|database| database.orders.values().any(|row| row.order.pending.is_some())).await;
    setup.venue_events.assert_empty();

    setup.crash_and_restart().await;
    let sent: v44::OrderCancelReplaceRequest = setup.venue_events.parse().await;
    assert_eq!((sent.orig_cl_ord_id.as_str(), sent.order_qty), (&*order, Some(150.into())));
    setup.venue_send(replaced(&sent.cl_ord_id, &order));
    setup.venue_send(fill_of(&sent.cl_ord_id, "E-2", 110, (150, 150)));
    let replaced = setup.events.execution_report().await;
    assert_eq!(
        terms(&replaced),
        (Some("C2".into()), Code::Known(v44::ExecType::Replaced), Some(150.into()), 40.into(), 110.into())
    );
    let filled = setup.events.execution_report().await;
    assert_eq!(
        terms(&filled),
        (Some("C2".into()), Code::Known(v44::ExecType::Trade), Some(150.into()), 150.into(), 0.into())
    );
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C2"), [(Status::Filled, 150.into())]);
}

/// An order the venue's session stored and sent, unanswered, then the session reset (as a
/// schedule resets it) while the gateway is down: the order is missing from the store, but it
/// was sent before the store was created, so the venue had it. It isn't sent again, and stays
/// pending: the venue's ack, when it comes, reaches the client.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_lost_to_a_store_reset_is_not_sent_again() {
    let mut setup = Setup::new().await;
    setup.send(order("C1"));
    let sent: v44::NewOrderSingle = setup.venue_events.parse().await;
    setup.until_stored(|database| !stored(database, "C1").is_empty()).await;

    setup.gateway.take().expect("the gateway is running").crash();
    setup.gone().await;
    setup.reset_venue_session().await;
    setup.start_again().await;
    setup.assert_quiet().await;
    setup.venue_send(acked(&sent.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C1"), Code::Known(v44::ExecType::New)));
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
}

/// The most common window: a fill's change committed, its hold released, but the venue's session
/// not yet storing the release when the gateway crashes (its store gated after the fill was
/// handed over and before the commit). The venue redelivers the fill, the book drops it as
/// stale, and the client sees nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn a_fill_committed_but_not_released_is_redelivered_and_dropped() {
    let mut setup = Setup::new().await;
    let downstream = setup.working_order("C1", 1).await;
    setup.gateway().committed().await;
    let writer = setup.gateway().store().hooks.gate.clone();
    writer.send_replace(false);
    setup.venue_send(fill(&downstream, "E-1", 40, 40));
    let partial = fill_seen(&setup.events.report().await);
    assert_eq!(partial, (setup.id(0, "1.2"), Some(40.into()), 40.into(), None));
    // Handed over and held: now the release can't be stored.
    setup.venue_gate.send_replace(false);
    writer.send_replace(true);
    setup.gateway().committed().await;

    setup.crash_and_restart().await;
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::PartiallyFilled, 40.into())]);
}

/// A client's cancel the database recorded but the venue's session never stored: after the
/// restart it goes to the venue under its ClOrdID, and the venue's Canceled answers the client.
#[tokio::test(flavor = "multi_thread")]
async fn a_clients_cancel_the_venue_never_got_goes_after_a_crash() {
    let mut setup = Setup::new().await;
    let order = setup.working_order("C1", 1).await;
    setup.venue_gate.send_replace(false);
    setup.send(cancel("C2", "C1"));
    let downstream = format!("{order}-1");
    setup.until_stored(|database| database.sent.contains_key(&*downstream)).await;
    setup.venue_events.assert_empty();

    setup.crash_and_restart().await;
    let msg = setup.venue_events.message().await;
    assert_eq!(msg.get(tags::POSS_RESEND), Some("Y"));
    let sent: v44::OrderCancelRequest = msg.parse().unwrap();
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), (&*downstream, &*order));
    setup.venue_send(canceled(&downstream, &order, 0));
    let canceled = setup.events.execution_report().await;
    assert_eq!(
        (canceled.cl_ord_id.as_deref(), canceled.orig_cl_ord_id.as_deref(), canceled.exec_type),
        (Some("C2"), Some("C1"), Code::Known(v44::ExecType::Canceled))
    );
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Canceled, Decimal::ZERO)]);
}

/// The gateway's own cancel (cancel on disconnect's) the database recorded but the venue's
/// session never stored: after the restart it goes to the venue, and the client, back, hears
/// the order is canceled.
#[tokio::test(flavor = "multi_thread")]
async fn a_gateway_cancel_the_venue_never_got_goes_after_a_crash() {
    let mut setup =
        Setup::with(Options { cancel_on_disconnect: Some(Duration::from_millis(50)), ..Options::default() }).await;
    let order = setup.working_order("C1", 1).await;
    setup.venue_gate.send_replace(false);
    setup.disconnect_client().await;
    let downstream = format!("{order}-1");
    setup.until_stored(|database| database.sent.contains_key(&*downstream)).await;
    setup.venue_events.assert_empty();

    setup.crash_and_restart().await;
    let msg = setup.venue_events.message().await;
    assert_eq!(msg.get(tags::POSS_RESEND), Some("Y"));
    let sent: v44::OrderCancelRequest = msg.parse().unwrap();
    assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), (&*downstream, &*order));
    setup.venue_send(canceled(&downstream, &order, 0));
    setup.reconnect_client().await;
    let canceled = setup.events.execution_report().await;
    assert_eq!((canceled.cl_ord_id.as_deref(), canceled.exec_type), (Some("C1"), Code::Known(v44::ExecType::Canceled)));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Canceled, Decimal::ZERO)]);
}

/// Neither the database nor the venue's session stored the order at the crash: the client's
/// redelivered order is booked afresh under the new start's ID and sent once, with no orphan to
/// cancel.
#[tokio::test(flavor = "multi_thread")]
async fn an_order_neither_stored_is_booked_afresh_after_a_crash() {
    let mut setup = Setup::new().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.venue_gate.send_replace(false);
    setup.send(order("C1"));
    let gateway = setup.gateway().service.gateway().clone();
    until(|| !gateway.unfinished().open_orders.is_empty()).await;
    drop(gateway);

    setup.crash_and_restart().await;
    let sent: v44::NewOrderSingle = setup.venue_events.parse().await;
    assert_eq!(sent.cl_ord_id.as_str(), setup.id(1, "1"));
    setup.venue_send(acked(&sent.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.order_id.as_str()), (Some("C1"), &*setup.id(1, "1")));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
}

/// An orphan handled after one crash isn't taken again after a second: the venue gets nothing
/// more.
#[tokio::test(flavor = "multi_thread")]
async fn an_orphan_handled_is_not_taken_again_at_the_next_restart() {
    let mut setup = Setup::new().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.send(order("C1"));
    let orphan: v44::NewOrderSingle = setup.venue_events.parse().await;
    setup.crash_and_restart().await;
    for _ in 0..2 {
        let msg = setup.venue_events.message().await;
        if let Ok(cancel) = msg.parse::<v44::OrderCancelRequest>() {
            assert_eq!(cancel.orig_cl_ord_id, orphan.cl_ord_id);
            setup.venue_send(canceled(&cancel.cl_ord_id, &orphan.cl_ord_id, 0));
        } else {
            let again: v44::NewOrderSingle = msg.parse().unwrap();
            setup.venue_send(acked(&again.cl_ord_id, 100));
        }
    }
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C1"), Code::Known(v44::ExecType::New)));
    setup.database().await;

    setup.crash_and_restart().await;
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
}

/// A request sent again after one crash isn't sent again after a second: the venue's session
/// stored it this time.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsent_request_sent_once_is_not_sent_again_at_the_next_restart() {
    let mut setup = Setup::new().await;
    setup.venue_gate.send_replace(false);
    setup.send(order("C1"));
    setup.until_stored(|database| !stored(database, "C1").is_empty()).await;
    setup.crash_and_restart().await;
    let sent: v44::NewOrderSingle = setup.venue_events.parse().await;
    assert_eq!(sent.cl_ord_id.as_str(), setup.id(0, "1"));
    setup.database().await;

    setup.crash_and_restart().await;
    setup.assert_quiet().await;
    setup.venue_send(acked(&sent.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C1"), Code::Known(v44::ExecType::New)));
}

/// A crash in the middle of the writer's transaction, its statements run and not committed: the
/// transaction goes with the connection, the database stays unlocked, and the order the venue's
/// session sent is an orphan, as if nothing had been written.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_inside_a_transaction_leaves_nothing_of_it() {
    let mut setup = Setup::new().await;
    setup.gateway().store().hooks.gate_in_transaction.send_replace(false);
    setup.send(order("C1"));
    let orphan: v44::NewOrderSingle = setup.venue_events.parse().await;

    setup.crash_and_restart().await;
    let mut cancelled = false;
    for _ in 0..2 {
        let msg = setup.venue_events.message().await;
        if let Ok(cancel) = msg.parse::<v44::OrderCancelRequest>() {
            assert_eq!(cancel.orig_cl_ord_id, orphan.cl_ord_id);
            cancelled = true;
            setup.venue_send(canceled(&cancel.cl_ord_id, &orphan.cl_ord_id, 0));
        } else {
            let again: v44::NewOrderSingle = msg.parse().unwrap();
            assert_eq!(again.cl_ord_id.as_str(), setup.id(1, "1"));
            setup.venue_send(acked(&again.cl_ord_id, 100));
        }
    }
    assert!(cancelled, "the orphan wasn't cancelled");
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.order_id.as_str()), (Some("C1"), &*setup.id(1, "1")));
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
}

/// Without a database: the client's order is held until the venue's session has stored it, so
/// a crash before then has the client redeliver it, and the new start, with an empty book,
/// books and sends it. The venue gets it once; the client hears one ack.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_database_an_order_the_venue_never_stored_is_redelivered_and_sent() {
    let mut setup = Setup::with(Options { database: false, ..Options::default() }).await;
    setup.venue_gate.send_replace(false);
    setup.send(order("C1"));
    let gateway = setup.gateway().service.gateway().clone();
    until(|| !gateway.unfinished().open_orders.is_empty()).await;
    drop(gateway);

    setup.crash_and_restart().await;
    let sent: v44::NewOrderSingle = setup.venue_events.parse().await;
    assert_eq!(sent.cl_ord_id.as_str(), setup.id(1, "1"));
    setup.venue_send(acked(&sent.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.order_id.as_str()), (Some("C1"), &*setup.id(1, "1")));
    setup.assert_quiet().await;
}

/// A venue whose session resets at each logon, as a new schedule period resets it: an order the
/// database has and the venue's session never stored, from before the reset, isn't sent into the
/// new session (a day order would go live a day late); the client hears it rejected.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsent_order_from_before_a_session_reset_is_rejected_not_sent() {
    let mut setup = Setup::with(Options { reset_on_logon: true, ..Options::default() }).await;
    setup.venue_gate.send_replace(false);
    setup.send(order("C1"));
    setup.until_stored(|database| !stored(database, "C1").is_empty()).await;

    setup.crash_and_restart().await;
    let rejected = setup.events.execution_report().await;
    assert_eq!(
        (rejected.cl_ord_id.as_deref(), rejected.exec_type.known()),
        (Some("C1"), Some(v44::ExecType::Rejected))
    );
    assert!(rejected.text.as_deref().is_some_and(|text| text.contains("reset")), "{rejected:?}");
    setup.assert_quiet().await;
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::Rejected, Decimal::ZERO)]);
}

/// A venue's three answered orders, a checkpoint, then a fourth order orphaned by a crash: the
/// restart's scan begins at the checkpoint, reading only what the store holds from there, and
/// still finds the orphan and cancels it.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_scans_from_the_checkpoint_and_still_cancels_an_orphan_after_it() {
    let mut setup = Setup::new().await;
    for n in 1..=3 {
        setup.working_order(&format!("C{n}"), n).await;
    }
    let checkpoint = setup.checkpoint().await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.send(order("C4"));
    let sent: v44::NewOrderSingle = setup.venue_events.parse().await;
    let orphan = setup.id(0, "4");
    assert_eq!(sent.cl_ord_id.as_str(), orphan);

    setup.gateway.take().expect("the gateway is running").crash();
    setup.gone().await;
    let ((_, whole), (holds, from_checkpoint)) =
        (setup.venue_store_read(1), setup.venue_store_read(checkpoint.next_seq));
    assert!(from_checkpoint < whole, "{checkpoint:?}, {holds} stored");
    setup.start_again().await;
    let found = setup.gateway().service.gateway().orphans_found();
    assert_eq!(found.scanned, from_checkpoint, "{found:?}, {checkpoint:?}, {holds} stored");
    assert_eq!(found.orders, 1, "{found:?}");
    let (mut cancel, mut again) = (None, None);
    for _ in 0..2 {
        let msg = setup.venue_events.message().await;
        match msg.msg_type() {
            MsgType::OrderCancelRequest => cancel = Some(msg.parse::<v44::OrderCancelRequest>().unwrap()),
            MsgType::NewOrderSingle => again = Some(msg.parse::<v44::NewOrderSingle>().unwrap()),
            other => panic!("unexpected {other:?}: {msg:?}"),
        }
    }
    let (cancel, again) = (cancel.expect("the orphan's cancel"), again.expect("the order again"));
    assert_eq!(cancel.orig_cl_ord_id.as_str(), orphan);
    setup.venue_send(canceled(&cancel.cl_ord_id, &orphan, 0));
    setup.venue_send(acked(&again.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C4"), Code::Known(v44::ExecType::New)));
    setup.assert_quiet().await;
}

/// Order C1 sent, unanswered, and its MsgSeqNum: its change committed.
async fn unanswered_order(setup: &mut Setup) -> (v44::NewOrderSingle, u64) {
    setup.send(order("C1"));
    let msg = setup.venue_events.message().await;
    let seq: u64 = msg.field(tags::MSG_SEQ_NUM).unwrap();
    setup.until_stored(|database| !stored(database, "C1").is_empty()).await;
    (msg.parse().unwrap(), seq)
}

/// After a restart, C1 wasn't sent again (nor anything else), and the venue's late ack of it
/// reaches the client.
async fn late_ack_applies(setup: &mut Setup, sent: &v44::NewOrderSingle) {
    let found = setup.gateway().service.gateway().orphans_found();
    assert_eq!(found.unsent, 0, "{found:?}");
    setup.assert_quiet().await;
    setup.venue_send(acked(&sent.cl_ord_id, 100));
    let new = setup.events.execution_report().await;
    assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C1"), Code::Known(v44::ExecType::New)));
    let database = setup.database().await;
    assert_eq!(stored(&database, "C1"), [(Status::New, Decimal::ZERO)]);
}

/// An order the venue hasn't answered holds the checkpoint back, though a later one is answered:
/// the restart's scan from the checkpoint still finds it stored, so it isn't sent again, and
/// stays pending.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_order_holds_the_checkpoint_back_across_a_restart() {
    let mut setup = Setup::new().await;
    let (sent, seq) = unanswered_order(&mut setup).await;
    setup.working_order("C2", 2).await;
    let checkpoint = setup.checkpoint().await;
    assert!(checkpoint.next_seq <= seq, "{checkpoint:?}, the order stored as {seq}");

    setup.crash_and_restart().await;
    late_ack_applies(&mut setup, &sent).await;
}

/// An order unanswered across a restart, restored from the database, holds back the next start's
/// checkpoints: a restart after one still finds it stored, so it isn't sent again.
#[tokio::test(flavor = "multi_thread")]
async fn an_order_unanswered_across_a_restart_holds_the_next_checkpoint_back() {
    let mut setup = Setup::new().await;
    let (sent, seq) = unanswered_order(&mut setup).await;
    setup.crash_and_restart().await;
    let checkpoint = setup.checkpoint().await;
    assert!(checkpoint.next_seq <= seq, "{checkpoint:?}, the order stored as {seq}");

    setup.crash_and_restart().await;
    late_ack_applies(&mut setup, &sent).await;
}

/// A venue whose session resets at each logon: a checkpoint taken before the reset is ignored
/// at the next start, though the new store is numbered past it, and the whole store is read.
#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_from_before_the_venues_store_was_reset_is_ignored() {
    let mut setup = Setup::with(Options { reset_on_logon: true, ..Options::default() }).await;
    for n in 1..=3 {
        setup.working_order(&format!("C{n}"), n).await;
    }
    let checkpoint = setup.checkpoint().await;
    setup.crash_and_restart().await;
    for n in 1..=4 {
        setup.working_order(&format!("D{n}"), 3 + n).await;
    }
    setup.database().await;

    setup.gateway.take().expect("the gateway is running").crash();
    setup.gone().await;
    let ((holds, whole), (_, from_checkpoint)) =
        (setup.venue_store_read(1), setup.venue_store_read(checkpoint.next_seq));
    assert!(checkpoint.next_seq <= holds + 1, "{checkpoint:?}, {holds} stored: the reset isn't seen by number");
    assert!(from_checkpoint < whole, "{checkpoint:?}, {holds} stored");
    setup.start_again().await;
    // Whole: a checkpoint taken since the reset would be honoured, and read less.
    let found = setup.gateway().service.gateway().orphans_found();
    assert_eq!(found.scanned, whole, "{found:?}, {checkpoint:?}, {holds} stored");
    setup.assert_quiet().await;
}

/// An orphaned replace, found at the restart and made pending on its order, holds back the
/// checkpoints after: its bound is the number the venue's session stored it under.
#[tokio::test(flavor = "multi_thread")]
async fn an_orphaned_replace_holds_the_checkpoint_back() {
    let mut setup = Setup::new().await;
    partly_filled(&mut setup).await;
    setup.gateway().store().hooks.gate.send_replace(false);
    setup.send(replace("C2", "C1", 150));
    let msg = setup.venue_events.message().await;
    let seq: u64 = msg.field(tags::MSG_SEQ_NUM).unwrap();
    let sent: v44::OrderCancelReplaceRequest = msg.parse().unwrap();

    setup.crash_and_restart().await;
    let found = setup.gateway().service.gateway().orphans_found();
    assert_eq!(found.amendments, 1, "{found:?}");
    let downstream = sent.cl_ord_id.to_string();
    let adopted = |database: &Snapshot| database.sent.get(&*downstream).is_some_and(|sent| sent.cl_ord_id == "C2");
    setup.until_stored(adopted).await;
    let checkpoint = setup.checkpoint().await;
    assert!(checkpoint.next_seq <= seq, "{checkpoint:?}, the replace stored as {seq}");
}
