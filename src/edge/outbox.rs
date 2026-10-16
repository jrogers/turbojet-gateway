//! Reports for clients that can't take them yet: disconnected, or reading more slowly than reports
//! arrive. Turbojet resends only what a session has stored, and refuses a message sent while the
//! session is disconnected or its send queue is full, so a report sent then would be lost. Here
//! each client gets a bounded queue, drained in order by a task of its own once the client can
//! take them.
//!
//! With a database, each report is also an `undelivered` row, written with the change that made
//! it; the outbox queues the row's deletion once the client's session has stored the report (or
//! it's lost), so a restart can send again what hadn't reached the client.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;
use tracing::{error, warn};
use turbojet::{Dropped, Message, Receipt, SendError, SessionHandle};

use super::writer::Writer;
use crate::book::ClientId;

/// Reports held for one client before it's logged out. A busy client sees a few thousand reports
/// a second at peak, so this rides out tens of seconds of a stalled or reconnecting client; at
/// about 1 KB a report, a client at the limit holds some 100 MB. A client further behind than
/// that isn't going to catch up, and logging it out says so rather than holding memory without end.
pub const MAX_OUTBOX: usize = 100_000;

/// Reports a client's drain task has queued on the session without yet hearing they're stored,
/// each with a copy kept to send again if the connection ends first. Waiting for each one's
/// receipt before queueing the next would cost a store commit per report; a window lets the
/// session commit them in groups. 256 is plenty for that, and bounds the copies. The copy costs
/// a clone (allocations) per report sent, the price of resending without Turbojet handing a
/// dropped message back.
const IN_FLIGHT: usize = 256;

/// Text(58) of the Logout sent to a client whose outbox is full.
const OVERFLOW_TEXT: &str = "too many undelivered reports";

/// Holds reports per client, and delivers them as each client can take them.
///
/// **Order.** A client receives its reports in the order [`deliver`](Self::deliver) was called
/// for it (calls racing on different threads have no order to keep). A report the connection
/// drops before storing it is sent again after the client logs back on, ahead of anything later;
/// one that was stored is resent by the session as usual.
///
/// **Loss.** A report is lost, with an error logged, only when the client's outbox is full (the
/// client is logged out too), when the session refuses it for good ([`Dropped::Rejected`]), or
/// when its store fails ([`Dropped::Storage`]: it may have been stored, and then it's resent from
/// there; sending it again could duplicate it).
///
/// **Limits.** Each client holds up to [`MAX_OUTBOX`] reports. Its queue and task are made on
/// first use and kept, so there's one per client session seen since start: every distinct session
/// that logs on over the process's life keeps a drain task here, and one that orders a `used`
/// entry in the book, until a restart. Listing the counterparties bounds their CompIDs but not
/// the SubIDs they log on with. Dropping the outbox aborts the drain tasks, and what they hold is
/// lost.
#[derive(Debug)]
pub struct Outbox {
    limit: usize,
    // ponytail: one lock for every client's push; per-client locks if it shows up in profiles.
    clients: Mutex<HashMap<ClientId, Client>>,
    tasks: Mutex<JoinSet<()>>,
    /// Deletes each report's `undelivered` row once it's finished with, if there's a database.
    writer: Option<Writer>,
}

impl Outbox {
    /// An outbox holding up to [`MAX_OUTBOX`] reports per client.
    pub(crate) fn new() -> Self {
        Self::with_limit(MAX_OUTBOX)
    }

    /// An outbox holding up to `limit` reports per client.
    pub(super) fn with_limit(limit: usize) -> Self {
        assert!(limit > 0, "an outbox must hold at least one report");
        Self { limit, clients: Mutex::default(), tasks: Mutex::default(), writer: None }
    }

    /// Deletes the `undelivered` row each report names as it's finished with: set it before any
    /// report is delivered.
    pub(crate) fn set_writer(&mut self, writer: Writer) {
        debug_assert!(lock(&self.clients).is_empty(), "the writer is set before any report is delivered");
        self.writer = Some(writer);
    }

    /// `client` logged on, on `session`: its held reports start going out. Call it from the
    /// acceptor's `Application::on_logon`.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime, for a client not seen before (its drain task is spawned).
    pub fn logged_on(&self, client: &ClientId, session: &SessionHandle) {
        let mut clients = lock(&self.clients);
        let client = self.client(&mut clients, client);
        *lock(&client.shared.session) = Some(session.clone());
        // Keeps a permit if the drain task isn't waiting yet: see `Drain::wait_for_logon`.
        client.shared.logon.notify_one();
    }

    /// Queues `msg` for `client` as [`deliver`](Self::deliver) does when something is already
    /// held: never sent straight away, so tests can hold reports up.
    #[cfg(test)]
    fn push(&self, client: &ClientId, msg: Message) -> bool {
        let mut clients = lock(&self.clients);
        let held = self.client(&mut clients, client);
        if !held.reserve(self.limit) {
            overflow(&held.shared, self.limit);
            return false;
        }
        held.enqueue(Entry { report: Report { msg, row: None }, receipt: None })
    }

    /// Sends `msg` to `client` straight away if nothing is held for it and its session takes it;
    /// otherwise queues it, to go out once everything held for it ahead of it has. Either way, if
    /// the connection drops it before it's stored, it's sent again after the client logs back on.
    /// Never waits. `false` if it was dropped: the client's outbox was full. The first report
    /// dropped logs an error and logs the client out; the rest are counted, and the count logged
    /// once the outbox empties.
    ///
    /// `row` is the report's `undelivered` row, if it has one: its deletion is queued once the
    /// session has stored the report, or it's lost after being taken here. Not if this returns
    /// `false`: the caller deletes it then.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime, for a client not seen before (its drain task is spawned).
    pub fn deliver(&self, client: &ClientId, msg: Message, row: Option<u64>) -> bool {
        let mut clients = lock(&self.clients);
        let held = self.client(&mut clients, client);
        // Something already held: queue behind it. Otherwise this one is now counted, so the next
        // queues behind it until its receipt is settled.
        if held.shared.pending.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            if !held.reserve(self.limit) {
                overflow(&held.shared, self.limit);
                return false;
            }
            return held.enqueue(Entry { report: Report { msg, row }, receipt: None });
        }
        let entry = match lock(&held.shared.session).as_ref() {
            Some(session) => {
                // A copy to send again if the receipt says the connection dropped it.
                let copy = msg.clone();
                match session.send(msg) {
                    Ok(receipt) => Entry { report: Report { msg: copy, row }, receipt: Some(receipt) },
                    Err(refused) => Entry { report: Report { msg: refused.into_message(), row }, receipt: None },
                }
            }
            None => Entry { report: Report { msg, row }, receipt: None },
        };
        held.enqueue(entry)
    }

    fn client<'a>(&self, clients: &'a mut HashMap<ClientId, Client>, id: &ClientId) -> &'a Client {
        clients.entry(id.clone()).or_insert_with(|| self.start(id))
    }

    fn start(&self, id: &ClientId) -> Client {
        // Holds at most `limit`, since `pending` counts what it holds and stops at `limit`.
        let (queue, receiver) = mpsc::channel(self.limit);
        let shared = Arc::new(Shared::new(id.clone()));
        let drain = Drain {
            shared: shared.clone(),
            writer: self.writer.clone(),
            queue: receiver,
            flight: VecDeque::new(),
            retry: VecDeque::new(),
        };
        lock(&self.tasks).spawn(drain.run());
        Client { queue, shared }
    }

    /// Reports held and not yet stored by the session, per client holding any.
    pub fn held_by_client(&self) -> BTreeMap<ClientId, usize> {
        let clients = lock(&self.clients);
        let held = clients.iter().map(|(id, c)| (id.clone(), c.shared.pending.load(Ordering::Acquire)));
        held.filter(|&(_, count)| count > 0).collect()
    }

    #[cfg(test)]
    fn held(&self, client: &ClientId) -> usize {
        lock(&self.clients).get(client).map_or(0, |c| c.shared.pending.load(Ordering::Acquire))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing is left half-changed under these locks, so a panic elsewhere doesn't spoil them.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drops a report for a full outbox. Only the first since the outbox was last empty logs an error
/// and logs the client out; the rest are counted, for [`Shared::settle`] to report.
fn overflow(shared: &Shared, limit: usize) {
    if shared.dropped.fetch_add(1, Ordering::AcqRel) > 0 {
        return;
    }
    error!(client = %shared.client, limit, "outbox full: dropping reports, logging the client out");
    if let Some(session) = lock(&shared.session).as_ref() {
        // Not connected: nothing to log out. Control queue full: a logout is already queued.
        let _ = session.logout(Some(OVERFLOW_TEXT));
    }
}

/// One client's end of its outbox.
#[derive(Debug)]
struct Client {
    queue: mpsc::Sender<Entry>,
    shared: Arc<Shared>,
}

impl Client {
    /// Counts one more report held, unless `limit` are already. Called under the clients lock,
    /// so the count can only fall between the check and the add.
    fn reserve(&self, limit: usize) -> bool {
        if self.shared.pending.load(Ordering::Acquire) >= limit {
            return false;
        }
        self.shared.pending.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Hands a report already counted to the drain task.
    fn enqueue(&self, entry: Entry) -> bool {
        match self.queue.try_send(entry) {
            Ok(()) => true,
            Err(e) => {
                debug_assert!(matches!(e, TrySendError::Closed(_)), "the queue holds all `pending` can count");
                // Closed: the drain task has ended, which it does only when the outbox is dropped.
                self.shared.settle();
                false
            }
        }
    }
}

#[derive(Debug)]
struct Shared {
    client: ClientId,
    /// The client's session, from its latest logon.
    session: Mutex<Option<SessionHandle>>,
    /// Signalled at each logon.
    logon: Notify,
    /// Reports pushed and not yet stored by the session or dropped: queued, held by the drain
    /// task, or in flight.
    pending: AtomicUsize,
    /// Reports dropped since the outbox was last empty, because it was full.
    dropped: AtomicUsize,
}

impl Shared {
    fn new(client: ClientId) -> Self {
        Self {
            client,
            session: Mutex::default(),
            logon: Notify::new(),
            pending: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }

    fn settle(&self) {
        let before = self.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(before > 0, "settled a report that wasn't counted");
        if before == 1 {
            let dropped = self.dropped.swap(0, Ordering::AcqRel);
            if dropped > 0 {
                warn!(client = %self.client, dropped, "outbox emptied; reports were dropped while it was full");
            }
        }
    }
}

/// A report, and its `undelivered` row if it has one.
#[derive(Debug)]
struct Report {
    msg: Message,
    row: Option<u64>,
}

#[derive(Debug)]
struct Entry {
    report: Report,
    /// Set when [`Outbox::deliver`] sent it straight away: the drain task watches it, to send the
    /// report again if the connection drops it.
    receipt: Option<Receipt>,
}

enum Step {
    Send(Entry),
    Settled(Result<u64, Dropped>),
    Closed,
}

/// Delivers one client's reports in order, on a task of its own.
struct Drain {
    shared: Arc<Shared>,
    writer: Option<Writer>,
    queue: mpsc::Receiver<Entry>,
    /// Queued on the session, oldest first, each with a copy to send again if it's dropped.
    flight: VecDeque<(Report, Receipt)>,
    /// To send before anything in `queue`, oldest first: dropped by a connection that ended. At
    /// most `IN_FLIGHT + 1`, as nothing more is taken from `queue` until it's empty.
    retry: VecDeque<Report>,
}

impl Drain {
    async fn run(mut self) {
        loop {
            debug_assert!(self.flight.len() + self.retry.len() <= IN_FLIGHT + 1);
            match self.next().await {
                Step::Send(Entry { report, receipt: Some(receipt) }) => {
                    debug_assert!(
                        self.flight.is_empty() && self.retry.is_empty(),
                        "sent straight away only when nothing was held"
                    );
                    self.flight.push_back((report, receipt));
                }
                Step::Send(Entry { report, receipt: None }) => self.send(report).await,
                Step::Settled(outcome) => self.settled(outcome).await,
                Step::Closed => return,
            }
        }
    }

    async fn next(&mut self) -> Step {
        if self.flight.len() >= IN_FLIGHT {
            return Step::Settled(oldest(&mut self.flight).await);
        }
        if let Some(report) = self.retry.pop_front() {
            return Step::Send(Entry { report, receipt: None });
        }
        // Settled first: a report dropped by a connection that ended must go back in `retry`
        // before anything newer is sent, perhaps on the next connection.
        tokio::select! {
            biased;
            outcome = oldest(&mut self.flight) => Step::Settled(outcome),
            entry = self.queue.recv() => entry.map_or(Step::Closed, Step::Send),
        }
    }

    async fn send(&mut self, Report { msg, row }: Report) {
        // Read at each send, not kept: always the latest logon's handle.
        let session = lock(&self.shared.session).clone();
        let copy = msg.clone();
        let sent = match &session {
            Some(session) => session.send_when_ready(msg).await,
            None => Err(SendError::NotConnected(msg)),
        };
        match sent {
            Ok(receipt) => self.flight.push_back((Report { msg: copy, row }, receipt)),
            Err(refused) => {
                // Not connected (or never logged on), or the connection ended while waiting for
                // room: what's in flight went on that connection, ahead of this.
                self.retry.push_front(Report { msg: refused.into_message(), row });
                self.requeue_flight().await;
                self.wait_for_logon().await;
            }
        }
    }

    async fn settled(&mut self, outcome: Result<u64, Dropped>) {
        let (report, _) = self.flight.pop_front().expect("settled the oldest in flight");
        match outcome {
            Ok(_) => self.finished(&report),
            Err(dropped @ (Dropped::Disconnected | Dropped::LoggingOut)) => {
                self.requeue_flight().await;
                self.retry.push_front(report);
                // Disconnected: the next send says whether there's a new connection yet. Logging
                // out, the session is still connected and would drop each send at once until it
                // disconnects; wait for the next logon rather than spin.
                if dropped == Dropped::LoggingOut {
                    self.wait_for_logon().await;
                }
            }
            Err(reason) => self.lost(&report, &reason),
        }
    }

    /// Settles everything in flight, putting what was dropped back at the front of `retry`, in
    /// order. A connection's queue is first in, first out, so once one report is dropped every
    /// later one on that connection is too.
    async fn requeue_flight(&mut self) {
        let mut dropped = Vec::with_capacity(self.flight.len());
        while let Some((report, receipt)) = self.flight.pop_front() {
            match receipt.await {
                Ok(_) => {
                    // A guard: receipts settle oldest first, so this means a connection stored a
                    // report after dropping an earlier one. Seen, it's a Turbojet change or bug:
                    // this report reached the client ahead of the ones now being resent.
                    warn!(client = %self.shared.client, "a report went out ahead of one dropped before it");
                    self.finished(&report);
                }
                Err(Dropped::Disconnected | Dropped::LoggingOut) => dropped.push(report),
                Err(reason) => self.lost(&report, &reason),
            }
        }
        for report in dropped.into_iter().rev() {
            self.retry.push_front(report);
        }
    }

    fn lost(&self, report: &Report, reason: &Dropped) {
        error!(client = %self.shared.client, %reason, report = ?report.msg, "report not delivered");
        self.finished(report);
    }

    /// Done with `report`: the session stored it, or it's lost. Either way a restart mustn't send
    /// it again, so its row goes.
    fn finished(&self, report: &Report) {
        if let (Some(writer), Some(row)) = (&self.writer, report.row) {
            writer.delivered(row);
        }
        self.shared.settle();
    }

    /// Waits for the client's next logon. `notify_one` keeps a permit when no one is waiting, so a
    /// logon between a failed send and this wait still wakes it.
    async fn wait_for_logon(&self) {
        self.shared.logon.notified().await;
    }
}

/// The outcome of the oldest report in flight; never, with none.
async fn oldest(flight: &mut VecDeque<(Report, Receipt)>) -> Result<u64, Dropped> {
    match flight.front_mut() {
        Some((_, receipt)) => receipt.await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tokio::time::timeout;
    use turbojet::message::tags;
    use turbojet::{
        Acceptor, Application, Context, Disconnect, Initiator, InitiatorConfig, MemoryStorage, MessageReject, MsgType,
        ReconnectPolicy, SessionConfig, SessionEventKind,
    };

    use super::*;

    /// Holds up a session in its next application message until released.
    #[derive(Default)]
    struct Hold {
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        holding: Notify,
    }

    impl Hold {
        /// Holds the next message; the returned sender releases it, sent to or dropped.
        fn arm(&self) -> std::sync::mpsc::Sender<()> {
            let (release, gate) = std::sync::mpsc::channel();
            *lock(&self.gate) = Some(gate);
            release
        }

        fn wait_if_armed(&self) {
            let gate = lock(&self.gate).take();
            if let Some(release) = gate {
                self.holding.notify_one();
                // Hands the worker's other duties, timers among them, to another thread.
                tokio::task::block_in_place(|| release.recv()).ok();
            }
        }

        async fn held(&self) {
            timeout(Duration::from_secs(5), self.holding.notified()).await.expect("not held");
        }
    }

    /// Turbojet Gateway's side: tells the outbox of each logon.
    struct Gateway {
        outbox: Arc<Outbox>,
        hold: Hold,
    }

    impl Application for Gateway {
        fn on_logon(&self, session: &SessionHandle) {
            self.outbox.logged_on(&ClientId::new(session.id()), session);
        }

        fn on_message(&self, _ctx: &mut Context<'_>, _msg: &Message) -> Result<(), MessageReject> {
            self.hold.wait_if_armed();
            Ok(())
        }
    }

    #[derive(Debug, PartialEq)]
    enum Event {
        LoggedOn,
        LoggedOut,
        Report(String),
    }

    /// The client's side: records what it sees.
    struct Recorder {
        events: UnboundedSender<Event>,
        hold: Hold,
    }

    impl Application for Recorder {
        fn on_logon(&self, _session: &SessionHandle) {
            let _ = self.events.send(Event::LoggedOn);
        }

        fn on_logout(&self, _session: &SessionHandle, _ended: Disconnect) {
            let _ = self.events.send(Event::LoggedOut);
        }

        fn on_message(&self, _ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
            self.hold.wait_if_armed();
            let _ = self.events.send(Event::Report(msg.get(tags::TEXT).unwrap_or_default().to_owned()));
            Ok(())
        }
    }

    const CLIENT: &str = "CLIENT";

    fn client() -> ClientId {
        CLIENT.into()
    }

    fn report(text: impl ToString) -> Message {
        Message::new(MsgType::ExecutionReport).with(tags::TEXT, text.to_string())
    }

    /// Turbojet Gateway's acceptor, serving on loopback, with an outbox holding `limit` per client.
    async fn gateway(limit: usize) -> (Arc<Outbox>, Arc<Gateway>, Acceptor, String) {
        let outbox = Arc::new(Outbox::with_limit(limit));
        let app = Arc::new(Gateway { outbox: outbox.clone(), hold: Hold::default() });
        let mut config = SessionConfig::new("FIX.4.4", "TURBOJETGATEWAY");
        // Small, so a held-up session fills it quickly.
        config.send_queue = 4;
        let acceptor = Acceptor::new(config, Arc::new(MemoryStorage::new()), app.clone()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(acceptor.clone().serve(listener));
        (outbox, app, acceptor, addr)
    }

    /// The client, a Turbojet initiator, not yet connected.
    fn client_session(addr: &str) -> (Initiator, Arc<Recorder>, UnboundedReceiver<Event>) {
        let (events, rx) = unbounded_channel();
        let recorder = Arc::new(Recorder { events, hold: Hold::default() });
        let mut config = InitiatorConfig::new(SessionConfig::new("FIX.4.4", CLIENT), "TURBOJETGATEWAY");
        config.reconnect = ReconnectPolicy::fixed(Duration::from_millis(50));
        let initiator = Initiator::new(addr, config, Arc::new(MemoryStorage::new()), recorder.clone()).unwrap();
        (initiator, recorder, rx)
    }

    /// The client, running, reconnecting on its own.
    fn start_client(addr: &str) -> (Initiator, Arc<Recorder>, UnboundedReceiver<Event>) {
        let (initiator, recorder, rx) = client_session(addr);
        tokio::spawn(initiator.clone().run());
        (initiator, recorder, rx)
    }

    /// One connection of the client's, over a stream the test can cut by aborting the task.
    async fn connect(initiator: &Initiator, addr: &str) -> tokio::task::JoinHandle<std::io::Result<()>> {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let info = turbojet::ConnectionInfo::new(stream.peer_addr().ok(), Vec::new());
        let initiator = initiator.clone();
        tokio::spawn(async move { initiator.run_stream(stream, info).await })
    }

    async fn next(events: &mut UnboundedReceiver<Event>) -> Event {
        timeout(Duration::from_secs(5), events.recv()).await.expect("timed out").expect("closed")
    }

    /// The next `n` events, each a report.
    async fn reports(events: &mut UnboundedReceiver<Event>, n: usize) -> Vec<Event> {
        let mut got = Vec::new();
        for _ in 0..n {
            got.push(next(events).await);
        }
        got
    }

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| i.to_string()).collect()
    }

    fn numbered_reports(n: usize) -> Vec<Event> {
        numbered(n).into_iter().map(Event::Report).collect()
    }

    /// Holds up Turbojet Gateway's session with the client, by having the client send a message
    /// that waits in `on_message` until the returned sender is dropped or sent to.
    async fn hold(app: &Gateway, initiator: &Initiator) -> std::sync::mpsc::Sender<()> {
        let release = app.hold.arm();
        initiator.handle().send(Message::new(MsgType::NewOrderSingle).with(tags::CL_ORD_ID, "hold")).unwrap();
        app.hold.held().await;
        release
    }

    #[tokio::test]
    async fn reports_held_while_disconnected_arrive_in_order_after_logon() {
        let (outbox, _app, _acceptor, addr) = gateway(MAX_OUTBOX).await;
        for i in 0..3 {
            assert!(outbox.push(&client(), report(i)));
        }
        let (_initiator, _recorder, mut events) = start_client(&addr);
        assert_eq!(next(&mut events).await, Event::LoggedOn);
        assert_eq!(reports(&mut events, 3).await, numbered_reports(3));
    }

    #[tokio::test]
    async fn deliver_sends_straight_away_when_connected_and_keeps_order() {
        let (outbox, _app, _acceptor, addr) = gateway(MAX_OUTBOX).await;
        let (_initiator, _recorder, mut events) = start_client(&addr);
        assert_eq!(next(&mut events).await, Event::LoggedOn);
        until(|| lock(&outbox.clients).get(&client()).is_some_and(|c| lock(&c.shared.session).is_some())).await;

        assert!(outbox.deliver(&client(), report(0), None));
        // Watched in flight until stored, so the next one queues behind it.
        assert_eq!(outbox.held(&client()), 1);
        assert_eq!(outbox.held_by_client(), BTreeMap::from([(client(), 1)]));
        assert!(outbox.deliver(&client(), report(1), None));
        assert!(outbox.push(&client(), report(2)));
        assert_eq!(reports(&mut events, 3).await, numbered_reports(3));
        until(|| outbox.held(&client()) == 0).await;
        assert!(outbox.held_by_client().is_empty());
    }

    #[tokio::test]
    async fn a_report_the_session_refuses_is_dropped_and_the_rest_go() {
        let (outbox, _app, _acceptor, addr) = gateway(MAX_OUTBOX).await;
        assert!(outbox.push(&client(), report(0)));
        // SOH inside a value: the session refuses it for good (Dropped::Rejected).
        assert!(outbox.push(&client(), report("bad\u{1}58=x")));
        assert!(outbox.push(&client(), report(1)));
        let (_initiator, _recorder, mut events) = start_client(&addr);
        assert_eq!(next(&mut events).await, Event::LoggedOn);
        assert_eq!(reports(&mut events, 2).await, numbered_reports(2));
        until(|| outbox.held(&client()) == 0).await;
        assert!(timeout(Duration::from_millis(200), events.recv()).await.is_err());
    }

    // Multi-threaded: the held-up session blocks a worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_full_outbox_logs_the_client_out_and_drops_the_report() {
        let (outbox, app, _acceptor, addr) = gateway(2).await;
        let (initiator, _recorder, mut events) = start_client(&addr);
        assert_eq!(next(&mut events).await, Event::LoggedOn);
        let release = hold(&app, &initiator).await;

        // Two held (the first in flight on the session that can't take it), the third dropped.
        let pushed = 2;
        for i in 0..pushed {
            assert!(outbox.push(&client(), report(i)));
        }
        assert!(!outbox.push(&client(), report(pushed)));
        // Dropped too; counted, not logged or logged out again.
        assert!(!outbox.push(&client(), report(pushed + 1)));
        release.send(()).unwrap();

        // Everything before the dropped one arrives, some perhaps after the client logs back on.
        let (mut got, mut logged_out, mut back_on) = (Vec::new(), false, false);
        while got.len() < pushed || !back_on {
            match next(&mut events).await {
                Event::Report(text) => got.push(text),
                Event::LoggedOut => logged_out = true,
                Event::LoggedOn => back_on = logged_out,
            }
        }
        assert_eq!(got, numbered(pushed));
        // The dropped ones never come.
        assert!(timeout(Duration::from_millis(200), events.recv()).await.is_err());
        assert_eq!(outbox.held(&client()), 0);
        // Emptied, the outbox reported the drops and starts counting afresh.
        assert_eq!(lock(&outbox.clients)[&client()].shared.dropped.load(Ordering::Acquire), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nothing_is_lost_when_the_connection_drops_while_draining() {
        const REPORTS: usize = 200;
        let (outbox, app, acceptor, addr) = gateway(MAX_OUTBOX).await;
        let (initiator, _recorder, mut events) = client_session(&addr);
        let connection = connect(&initiator, &addr).await;
        assert_eq!(next(&mut events).await, Event::LoggedOn);

        // With the session held up, at most its send queue and one waiting for room have left the
        // outbox when the connection is cut.
        let release = hold(&app, &initiator).await;
        for i in 0..REPORTS {
            assert!(outbox.push(&client(), report(i)));
        }
        let _connection = cut_and_reconnect(connection, release, &acceptor, &initiator, &addr, &mut events).await;
        // Those the session stored come back as resends; the outbox sends the rest again.
        assert_eq!(reports(&mut events, REPORTS).await, numbered_reports(REPORTS));
        assert!(timeout(Duration::from_millis(200), events.recv()).await.is_err());
        assert_eq!(outbox.held(&client()), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_report_delivered_straight_away_and_then_dropped_is_sent_again_in_order() {
        let (outbox, _app, acceptor, addr) = gateway(MAX_OUTBOX).await;
        let mut sessions = acceptor.subscribe();
        let (_initiator, recorder, mut events) = start_client(&addr);
        assert_eq!(next(&mut events).await, Event::LoggedOn);
        until(|| lock(&outbox.clients).get(&client()).is_some_and(|c| lock(&c.shared.session).is_some())).await;

        // The client, held up, doesn't answer Turbojet Gateway's Logout, so the session stays
        // logging out: it takes the report sent straight away, and its receipt says LoggingOut.
        let release = recorder.hold.arm();
        acceptor.session(CLIENT).send(report("before")).unwrap();
        recorder.hold.held().await;
        acceptor.session(CLIENT).logout(Some("bye")).unwrap();
        let logging_out = async { while sessions.recv().await.unwrap().kind != SessionEventKind::LoggingOut {} };
        timeout(Duration::from_secs(5), logging_out).await.expect("never logging out");
        assert!(outbox.deliver(&client(), report(0), None));
        assert_eq!(outbox.held(&client()), 1);
        assert!(outbox.push(&client(), report(1)));
        release.send(()).unwrap();

        assert_eq!(next(&mut events).await, Event::Report("before".into()));
        assert_eq!(next(&mut events).await, Event::LoggedOut);
        assert_eq!(next(&mut events).await, Event::LoggedOn);
        assert_eq!(reports(&mut events, 2).await, numbered_reports(2));
        assert!(timeout(Duration::from_millis(200), events.recv()).await.is_err());
        until(|| outbox.held(&client()) == 0).await;
    }

    /// Cuts the client's held-up connection, releases Turbojet Gateway's session, and reconnects
    /// once Turbojet Gateway has noticed (it refuses a Logon until then). The client sees nothing
    /// in between.
    async fn cut_and_reconnect(
        connection: tokio::task::JoinHandle<std::io::Result<()>>,
        release: std::sync::mpsc::Sender<()>,
        acceptor: &Acceptor,
        initiator: &Initiator,
        addr: &str,
        events: &mut UnboundedReceiver<Event>,
    ) -> tokio::task::JoinHandle<std::io::Result<()>> {
        connection.abort();
        let _ = connection.await;
        release.send(()).unwrap();
        assert_eq!(next(events).await, Event::LoggedOut);
        assert!(events.try_recv().is_err());
        until(|| !acceptor.session(CLIENT).is_connected()).await;
        let connection = connect(initiator, addr).await;
        assert_eq!(next(events).await, Event::LoggedOn);
        connection
    }

    /// Waits for `done`, for up to five seconds.
    async fn until(done: impl Fn() -> bool) {
        let wait = async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        timeout(Duration::from_secs(5), wait).await.expect("timed out waiting");
    }
}
