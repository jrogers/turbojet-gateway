//! The gateway's edges: what connects the book to client and venue sessions.
//!
//! One [`Gateway`] holds the book. [`ClientApp`] serves the acceptor the clients log on to, and
//! [`VenueApp`] the initiators that log on to venues. Both call into the book inline, under its
//! lock, and send what it answers before letting go: reports through the [`Outbox`], venue
//! requests through each venue's [`SessionHandle`]. Neither waits, so the lock is held for
//! microseconds, and every client gets its reports in the order the book made them, even when
//! its own request and a venue's fill race on different threads.
//!
//! With a database, each input's changes to the book, and the reports it queued for clients, go
//! to the [`Writer`] in the order the book made them, and the inbound message they came from is
//! held in flight until they've committed: see [`writer`].

pub mod orphans;
pub mod outbox;
pub mod writer;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, NaiveTime, Utc};
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use turbojet::codec;
use turbojet::fields::{BusinessRejectReason, CompactString, UtcTimestamp, format_compact};
use turbojet::message::tags;
use turbojet::{
    Application, Clock, ConnectionInfo, Context, Disconnect, Dropped, Hold, Message, MessageReject, Receipt, SendError,
    SessionHandle, SessionId, SessionSchedule,
};

use crate::book::{
    Alarm, AlarmKind, Book, ClientId, Day, GatewayCancel, GatewayEvent, Input, OrderDetail, OrderFilter, OrderId,
    OrderView, Output, RiskView, Scope, ScopeName, Snapshot, Undelivered, VenueId,
};
use crate::fix::{self, Version};
use crate::risk;
use crate::router::Router;
pub use orphans::Orphans;
pub use outbox::Outbox;
pub use writer::Writer;

/// Text(58) of the reject for an order whose venue isn't connected, or isn't configured.
const VENUE_UNAVAILABLE: &str = "venue unavailable";

/// Text(58) of the reject for an order whose venue's send queue is full.
const VENUE_BUSY: &str = "venue unavailable: its send queue is full";

/// Sessions one CompID may log on with (told apart by SubID or LocationID). Each keeps a report
/// queue and task, and its own ClOrdID budget in the book, until a restart, so the listed
/// counterparties bound the gateway's memory only if their sessions are bounded too. A firm
/// uses a few desks a CompID; 16 leaves room without letting one client cycle SubIDs unchecked.
pub const MAX_SESSIONS_PER_COMP_ID: usize = 16;

/// How often [`Gateway::run_ticks`] looks for the gateway's cancels a connected venue's session
/// refused (its queue full, say), and for client sessions whose trading day has ended. A venue
/// that's down gets the cancels when it logs on; one that's up but was busy gets them within a
/// second, which a kill switch can wait, without hammering a session that's already behind. A
/// quiet session's day rolls within a second of its end.
pub const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Events [`Gateway::subscribe`]'s channel holds for its slowest listener: an order's few events,
/// times a burst of orders. A listener further behind gets `Lagged` and reads the book afresh
/// (the API's live socket sends a snapshot) rather than the gateway waiting on it.
pub const EVENTS_CAPACITY: usize = 4_096;

/// Each client CompID's schedule, if it has one (the sessions file's, say): its sessions' trading
/// day ends with the schedule's period, or at `day_end` within it. See [`Gateway::with_days`].
#[derive(Clone)]
pub struct Schedules(pub Arc<ScheduleOf>);

/// A client CompID's schedule, if it has one.
pub type ScheduleOf = dyn Fn(&str) -> Option<SessionSchedule> + Send + Sync;

impl fmt::Debug for Schedules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Schedules")
    }
}

/// The book, and where its outputs go.
#[derive(Debug)]
pub struct Gateway {
    state: Mutex<State>,
    outbox: Outbox,
    /// Each client session seen since start, made once: see [`client_of`](Self::client_of).
    clients: Mutex<HashMap<SessionId, ClientId>>,
    /// Clients the store had at the start, by session as it shows, until each logs on: counted
    /// against their CompID's sessions as `clients` are. Locked after `clients`.
    restored: Mutex<HashMap<String, ClientId>>,
    /// Every client's: the acceptor's BeginString.
    client_version: Version,
    /// For TransactTime(60) on what the gateway sends.
    clock: Clock,
    /// A refused send left a gateway cancel waiting since the last retry tick (see
    /// [`Book::take_cancel_unsent`]): the tick looks at the book only then.
    cancels_waiting: AtomicBool,
    /// Commits the book's changes, if there's a database: see [`with_writer`](Self::with_writer).
    writer: Option<Writer>,
    /// See [`subscribe`](Self::subscribe).
    events: broadcast::Sender<GatewayEvent>,
    /// Order views built for events, to show none are without a listener.
    #[cfg(test)]
    views_built: std::sync::atomic::AtomicUsize,
}

/// Why [`Gateway::cancel_order`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelError {
    /// The book doesn't hold the order: it never did, or it was done and dropped at a roll.
    UnknownOrder,
    /// The order is filled, canceled, expired or rejected.
    Done,
}

impl fmt::Display for CancelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownOrder => "unknown order",
            Self::Done => "the order is done",
        })
    }
}

impl std::error::Error for CancelError {}

/// Why [`Gateway::request_status`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusError {
    /// As [`CancelError::UnknownOrder`].
    UnknownOrder,
    /// As [`CancelError::Done`].
    Done,
    /// The order's venue session isn't connected, or its send queue is full.
    VenueUnavailable,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownOrder => "unknown order",
            Self::Done => "the order is done",
            Self::VenueUnavailable => "the order's venue isn't logged on or can't take the request",
        })
    }
}

impl std::error::Error for StatusError {}

/// Why [`Gateway::resolve`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// As [`CancelError::UnknownOrder`].
    UnknownOrder,
    /// As [`CancelError::Done`].
    Done,
    /// The order's venue hasn't said it doesn't know it: see [`Book::is_unknown_at_venue`].
    NotUnknownAtVenue,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownOrder => "unknown order",
            Self::Done => "the order is done",
            Self::NotUnknownAtVenue => {
                "the venue hasn't said it doesn't know the order; ask it first (POST .../status)"
            }
        })
    }
}

impl std::error::Error for ResolveError {}

/// What the gateway leaves unfinished at a stop: see [`Gateway::unfinished`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    /// Open orders, per venue holding any.
    pub open_orders: BTreeMap<VenueId, usize>,
    /// Reports not yet delivered, per client holding any.
    pub undelivered: BTreeMap<ClientId, usize>,
    /// Orders the gateway is still cancelling itself, stuck ones too: see [`Book::cancelling`].
    pub cancelling: Vec<(OrderId, GatewayCancel)>,
}

/// Everything sending needs, under one lock so reports leave in the book's order.
#[derive(Debug)]
struct State {
    book: Book,
    venues: HashMap<VenueId, Venue>,
    /// Which venue each initiator's session is.
    venue_names: HashMap<SessionId, (VenueId, Version)>,
    /// When a client session with no schedule ends its day: a schedule whose periods run from
    /// one end of day to the next.
    day_end: SessionSchedule,
    schedules: Schedules,
    /// Cancels for orders a venue's session sent that the store never booked, by venue, to send
    /// at its next logon: see [`orphans`].
    orphan_cancels: HashMap<VenueId, Vec<orphans::OrphanCancel>>,
    /// Those cancels sent, waiting for the venue's session to store them, before their orders
    /// are recorded or the cancels queued again.
    orphan_cancels_sending: usize,
    /// Those orders' downstream ClOrdIDs, and their cancels': the venue's reports under them are
    /// logged, not handed to the book.
    orphan_ids: HashSet<CompactString>,
    /// Requests the store has as sent that each venue's session never stored, to send at its next
    /// logon: see [`orphans`].
    unsent: HashMap<VenueId, orphans::Unsent>,
}

#[derive(Debug)]
struct Venue {
    version: Version,
    session: SessionHandle,
}

impl Gateway {
    /// A gateway routing by `router`, whose clients speak `client_version`, with no venues until
    /// [`set_venues`](Self::set_venues).
    pub fn new(router: Router, client_version: Version) -> Self {
        let state = State {
            book: Book::new(router),
            venues: HashMap::new(),
            venue_names: HashMap::new(),
            day_end: SessionSchedule::daily(NaiveTime::MIN, NaiveTime::MIN),
            schedules: Schedules(Arc::new(|_| None)),
            orphan_cancels: HashMap::new(),
            orphan_cancels_sending: 0,
            orphan_ids: HashSet::new(),
            unsent: HashMap::new(),
        };
        Self {
            state: Mutex::new(state),
            outbox: Outbox::new(),
            clients: Mutex::default(),
            restored: Mutex::default(),
            client_version,
            clock: Clock::system(),
            cancels_waiting: AtomicBool::new(false),
            writer: None,
            events: broadcast::channel(EVENTS_CAPACITY).0,
            #[cfg(test)]
            views_built: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Commits every change to the book, and every report queued for a client until its session
    /// has stored it, through `writer`, holding each inbound message until its changes have
    /// committed. Without one, the book is kept in memory only.
    #[must_use]
    pub fn with_writer(mut self, writer: Writer) -> Self {
        self.outbox.set_writer(writer.clone());
        self.writer = Some(writer);
        self
    }

    /// Starts every ID the book issues with `prefix`, such as [`start_prefix`](crate::book::start_prefix)'s.
    #[must_use]
    pub fn with_id_prefix(mut self, prefix: CompactString) -> Self {
        self.state.get_mut().expect("nothing has used the gateway yet").book.set_prefix(prefix);
        self
    }

    /// Checks orders against `limits`.
    #[must_use]
    pub fn with_limits(mut self, limits: risk::Config) -> Self {
        let book = &mut self.state.get_mut().expect("nothing has used the gateway yet").book;
        let router = book.router().clone();
        book.set_config(router, limits);
        self
    }

    /// Each client session's trading day ends at `day_end`'s period ends (see
    /// [`config::day_end`](crate::config::day_end)), or sooner when its CompID's schedule's period
    /// ends, from `schedules`. By default every session's day ends at midnight UTC, and none has a
    /// schedule. See [`Input::DaysStarted`] for what a new day changes.
    #[must_use]
    pub fn with_days(mut self, day_end: SessionSchedule, schedules: Schedules) -> Self {
        let state = self.state.get_mut().expect("nothing has used the gateway yet");
        (state.day_end, state.schedules) = (day_end, schedules);
        self
    }

    /// Takes the time from `clock`: TransactTime(60), and when trading days end.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Fills the book from `snapshot`, a store's [`load`](crate::store::Store::load), and queues
    /// the reports it holds undelivered for their clients again, marked PossResend(97), under
    /// their rows, which go once each client's session has stored its report. Call it once, after
    /// the `with_` calls (the prefix, limits and writer set) and before any session starts.
    /// Restored orders keep their names; new ones take this start's prefix.
    ///
    /// # Panics
    ///
    /// If the book isn't new, or the snapshot holds what a book couldn't (see
    /// [`Book::restore`]); outside a tokio runtime, with reports to queue.
    pub fn restore(&self, mut snapshot: Snapshot) {
        let undelivered = std::mem::take(&mut snapshot.undelivered);
        let restored = restored_clients(&snapshot, undelivered.values().map(|row| &row.client));
        let orders = snapshot.orders.len();
        {
            let mut state = self.lock();
            state.book.restore(snapshot);
            info!(
                orders,
                open_orders = ?state.book.open_orders_by_venue(),
                blocked = ?state.book.blocked(),
                clients = restored.len(),
                undelivered = undelivered.len(),
                "restored the book from the database"
            );
        }
        *self.restored.lock().unwrap_or_else(PoisonError::into_inner) =
            restored.into_iter().map(|client| (client.to_string(), client)).collect();
        for (row, Undelivered { client, message }) in undelivered {
            let msg = match codec::decode(&message) {
                codec::Decoded::Message(msg, _) => msg.with(tags::POSS_RESEND, true),
                other => {
                    error!(%client, row, ?other, "an undelivered report doesn't decode; dropped");
                    self.delivered(row);
                    continue;
                }
            };
            if !self.outbox.deliver(&client, msg, Some(row)) {
                self.delivered(row);
            }
        }
    }

    /// Deletes undelivered row `row`, if there's a database.
    fn delivered(&self, row: u64) {
        if let Some(writer) = &self.writer {
            writer.delivered(row);
        }
    }

    /// Ends the sessions' days at `day_end`'s period ends from each one's next new day on: the
    /// day each is in ends when it was going to.
    pub fn set_day_end(&self, day_end: SessionSchedule) {
        self.lock().day_end = day_end;
    }

    /// What every ID the book issues starts with.
    pub fn id_prefix(&self) -> CompactString {
        self.lock().book.prefix().clone()
    }

    /// The venues, by name (`[initiator.NAME]`), each with its initiator's session; replaces any
    /// set before. Call it once the initiators are made, and again after a reload changes them.
    /// Until a venue is set, its session's logons are refused (see [`VenueApp`]) and orders for
    /// it are rejected as unavailable.
    ///
    /// A venue whose session has a FIX version the gateway doesn't convert is skipped, with an
    /// error logged, so one misconfigured venue doesn't keep the others from changing. Returns
    /// the skipped venues' names.
    pub fn set_venues(&self, venues: impl IntoIterator<Item = (VenueId, SessionHandle)>) -> Vec<VenueId> {
        let mut by_name = HashMap::new();
        let mut names = HashMap::new();
        let mut skipped = Vec::new();
        for (name, session) in venues {
            let version = match session.id().begin_string.parse::<Version>() {
                Ok(version) => version,
                Err(e) => {
                    error!(venue = %name, "skipping the venue: {e}");
                    skipped.push(name);
                    continue;
                }
            };
            names.insert(session.id().clone(), (name.clone(), version));
            by_name.insert(name, Venue { version, session });
        }
        let mut state = self.lock();
        state.venues = by_name;
        state.venue_names = names;
        skipped
    }

    /// The router new orders are routed by.
    pub fn router(&self) -> Router {
        self.lock().book.router().clone()
    }

    /// What a stop leaves unfinished: open orders, undelivered reports and the gateway's own
    /// cancels. Without a database a stop loses them; with one, a restart restores them.
    pub fn unfinished(&self) -> Unfinished {
        let state = self.lock();
        Unfinished {
            open_orders: state.book.open_orders_by_venue(),
            undelivered: self.outbox.held_by_client(),
            cancelling: state.book.cancelling(),
        }
    }

    /// Blocks the clients in `scope`: their new orders are rejected until [`unblock`](Self::unblock)
    /// lifts this scope (see [`Input::Block`]). Their working orders stay; see
    /// [`cancel_all`](Self::cancel_all). `false` if `scope` was blocked already, which changes
    /// nothing. Returns once the block is committed, with a database, either way: a block made
    /// by an earlier call may still be on its way.
    pub async fn block(self: &Arc<Self>, scope: Scope) -> bool {
        let changed = {
            let mut state = self.lock();
            let changed = !state.book.blocked().contains(&scope);
            if changed {
                warn!(?scope, "blocking clients");
                self.handle_locked(&mut state, [Input::Block { scope }], None);
            } else {
                info!(?scope, "blocked already");
            }
            changed
        };
        self.committed().await;
        changed
    }

    /// The kill switch: blocks the clients in `scope` and cancels their live orders at the
    /// venues, as one step, so no order slips in between. Each client hears of each order's end
    /// as the venue's Canceled comes back. [`cancelling`](Self::cancelling) lists the cancels
    /// still going. Returns once the block and the cancels sent are committed, with a database.
    pub async fn cancel_all(self: &Arc<Self>, scope: Scope) {
        warn!(?scope, "blocking clients and cancelling their orders");
        self.handle_all([Input::Block { scope: scope.clone() }, Input::CancelAll { scope }]);
        self.committed().await;
    }

    /// Lifts the block of exactly `scope`: a block of a session's CompID, or of everyone, stays.
    /// `false` if `scope` wasn't blocked, which changes nothing. Returns once the unblock is
    /// committed, with a database, either way, as [`block`](Self::block) does.
    pub async fn unblock(self: &Arc<Self>, scope: Scope) -> bool {
        let changed = {
            let mut state = self.lock();
            let changed = state.book.blocked().contains(&scope);
            if changed {
                warn!(?scope, "unblocking clients");
                self.handle_locked(&mut state, [Input::Unblock { scope }], None);
            } else {
                info!(?scope, blocked = ?state.book.blocked(), "nothing to unblock");
            }
            changed
        };
        self.committed().await;
        changed
    }

    /// The gateway's own cancel of one order, as [`cancel_all`](Self::cancel_all) sends for each,
    /// without a block. A stuck cancel starts again with its tries counted afresh, which makes
    /// this the way to retry one; one in flight is left to finish. Returns once the cancel sent is
    /// committed, with a database.
    ///
    /// # Errors
    ///
    /// If the book doesn't hold the order, or it's done.
    pub async fn cancel_order(self: &Arc<Self>, order: OrderId) -> Result<(), CancelError> {
        {
            let mut state = self.lock();
            let found = state.book.order(order).ok_or(CancelError::UnknownOrder)?;
            if found.status.is_done() {
                return Err(CancelError::Done);
            }
            warn!(order = %found.name, client = %found.client, "cancelling an order");
            self.handle_locked(&mut state, [Input::CancelOrder { order }], None);
        }
        self.committed().await;
        Ok(())
    }

    /// Asks the order's venue where the order stands: see [`Input::VenueStatusRequest`]. Returns
    /// once the venue's session has queued the request. Nothing is held or written, since nothing
    /// changes until the venue answers, and nothing waits for a venue that's down: ask again once
    /// it's up.
    ///
    /// # Errors
    ///
    /// If the book doesn't hold the order, it's done, or its venue's session can't take the
    /// request.
    pub fn request_status(&self, order: OrderId) -> Result<(), StatusError> {
        let mut state = self.lock();
        let state = &mut *state;
        let found = state.book.order(order).ok_or(StatusError::UnknownOrder)?;
        if found.status.is_done() {
            return Err(StatusError::Done);
        }
        let venue = found.venue.clone();
        // Not through `handle_locked`: its failed sends go back to the book as `SendFailed`, which
        // under the order's own ClOrdID would reject it, and there's nothing to commit.
        let (had_changes, mut out) = (state.book.has_changes(), Vec::new());
        state.book.handle(Input::VenueStatusRequest { order }, &mut out);
        debug_assert!(had_changes || !state.book.has_changes(), "a status request changes nothing");
        let [output] = <[Output; 1]>::try_from(out).expect("a live order's status request is sent");
        send_status_request(state.venues.get(&venue), &output, self.clock.now().into())
    }

    /// Cancels an order its venue has said it doesn't know: see [`Input::Resolve`]. Returns once
    /// the cancel is committed, with a database.
    ///
    /// # Errors
    ///
    /// If the book doesn't hold the order, it's done, or its venue hasn't said it doesn't know it.
    pub async fn resolve(self: &Arc<Self>, order: OrderId) -> Result<(), ResolveError> {
        {
            let mut state = self.lock();
            let found = state.book.order(order).ok_or(ResolveError::UnknownOrder)?;
            if found.status.is_done() {
                return Err(ResolveError::Done);
            }
            if !state.book.is_unknown_at_venue(order) {
                return Err(ResolveError::NotUnknownAtVenue);
            }
            self.handle_locked(&mut state, [Input::Resolve { order }], None);
        }
        self.committed().await;
        Ok(())
    }

    /// The client whose session shows as `session`, if it has logged on since start or the book
    /// had it at the start.
    pub fn client(&self, session: &str) -> Option<ClientId> {
        let clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        // ponytail: a scan of every session; a map by session string if there are many.
        if let Some(client) = clients.values().find(|client| client.session() == session) {
            return Some(client.clone());
        }
        self.restored.lock().unwrap_or_else(PoisonError::into_inner).get(session).cloned()
    }

    /// The scope `name` names; `None` for a session the gateway doesn't know (see
    /// [`client`](Self::client)).
    pub fn scope(&self, name: &ScopeName) -> Option<Scope> {
        match name {
            ScopeName::Session(session) => self.client(session).map(Scope::Session),
            ScopeName::CompId(comp_id) => Some(Scope::CompId(comp_id.clone())),
            ScopeName::All => Some(Scope::All),
        }
    }

    /// The order whose OrderID(37) is `name`, if the book holds it.
    pub fn order_id(&self, name: &str) -> Option<OrderId> {
        self.lock().book.order_named(name)
    }

    /// The order `client` knows by `cl_ord_id`, if the book holds it.
    pub fn order_by_cl_ord_id(&self, client: &ClientId, cl_ord_id: &str) -> Option<OrderId> {
        self.lock().book.order_by_cl_ord_id(client, cl_ord_id)
    }

    /// A page of the book's orders: see [`Book::orders`].
    pub fn orders(&self, filter: &OrderFilter) -> Vec<OrderView> {
        self.lock().book.orders(filter)
    }

    /// One order and the requests sent for it, if the book holds it.
    pub fn order(&self, id: OrderId) -> Option<OrderDetail> {
        self.lock().book.order_detail(id)
    }

    /// The limits in force and what each session counts against them: see [`Book::risk_view`].
    pub fn risk(&self) -> RiskView {
        self.lock().book.risk_view()
    }

    /// Every order as each step leaves it, blocks, unblocks and alarms, from now on, in the order
    /// the book made them. Order views are built only while someone listens. Alarms raised outside
    /// the book (the startup scan, venue logons) aren't ordered against the book's steps.
    pub fn subscribe(&self) -> broadcast::Receiver<GatewayEvent> {
        self.events.subscribe()
    }

    /// Whether new orders are rejected because the database is too far behind: see
    /// [`AlarmKind::GatewayUnavailable`].
    pub fn unavailable(&self) -> bool {
        self.lock().book.unavailable()
    }

    /// The working orders (a page at most: see [`Book::orders`]), of the clients of `comp_ids` if
    /// given, the scopes blocked, and whether new orders are rejected, read together: a live view's
    /// snapshot.
    pub fn working_view(&self, comp_ids: Option<Vec<CompactString>>) -> (Vec<OrderView>, Vec<Scope>, bool) {
        let state = self.lock();
        let orders = state.book.orders(&OrderFilter { working: true, comp_ids, ..OrderFilter::default() });
        (orders, state.book.blocked().to_vec(), state.book.unavailable())
    }

    /// Publishes an alarm the gateway raises outside the book, if anyone listens: about no
    /// client's order.
    fn alarm(&self, kind: AlarmKind, text: impl FnOnce() -> String) {
        if self.events.receiver_count() > 0 {
            // An error only once every listener has gone since the check.
            let _ = self.events.send(GatewayEvent::Alarm(Alarm {
                time: self.clock.now(),
                kind,
                client: None,
                text: text(),
            }));
        }
    }

    /// Completes once every change to the book so far has committed; at once without a database.
    pub async fn committed(&self) {
        if let Some(writer) = &self.writer {
            writer.committed().await;
        }
    }

    /// The scopes blocked now.
    pub fn blocked(&self) -> Vec<Scope> {
        self.lock().book.blocked().to_vec()
    }

    /// Orders the gateway is cancelling itself: see [`Book::cancelling`].
    pub fn cancelling(&self) -> Vec<(OrderId, GatewayCancel)> {
        self.lock().book.cancelling()
    }

    /// See [`Book::cancels_settled`].
    pub fn cancels_settled(&self) -> bool {
        self.lock().book.cancels_settled()
    }

    /// At shutdown, the clients gone: cancels the working orders of each client session whose
    /// CompID `on` says has cancel on disconnect, whatever its trigger, without a block, as
    /// [`ClientApp::on_cancel_on_disconnect`] does. Turbojet starts no countdown for a session our
    /// side ends, and these orders would otherwise stay working with no gateway to report to.
    /// Logged as one warning with each session's working orders, counted under the same lock.
    pub fn cancel_at_shutdown(self: &Arc<Self>, on: impl Fn(&str) -> bool) {
        let mut state = self.lock();
        let mut orders = state.book.working_by_client();
        orders.retain(|client, _| on(client.comp_id()));
        if orders.is_empty() {
            return;
        }
        warn!(working = ?orders, "shutting down: cancelling the working orders of clients with cancel on disconnect");
        let inputs = orders.into_keys().map(|client| Input::CancelAll { scope: Scope::Session(client) });
        self.handle_locked(&mut state, inputs, None);
    }

    /// Routes and checks new orders by `router` and `limits` from now on, both changed under one
    /// lock so no order sees one without the other.
    pub fn set_config(&self, router: Router, limits: risk::Config) {
        self.lock().book.set_config(router, limits);
    }

    /// The risk limits new orders are checked against.
    pub fn limits(&self) -> risk::Config {
        self.lock().book.limits().clone()
    }

    /// Whether the acceptor's session `id` may log on: a session seen before may; a new one if
    /// its CompID has fewer than [`MAX_SESSIONS_PER_COMP_ID`] and nothing the client names in its
    /// Logon (its CompID, SubID and LocationID, and the SubID and LocationID it addresses us by)
    /// has a '/', ':' or '>'. The session's printed name is how a restored client and the message
    /// log know it, so it must name one session only: with a '/', `FUNDA/X` and `FUNDA` SubID `X`
    /// print the same, with a ':' `FUNDA:Q` and `FUNDA` with qualifier `Q`, and with a '>' (of
    /// `->`) our SubID `S->T` to CompID `U` and our SubID `S` to CompID `T->U`. Records the
    /// session, checked and added under one lock.
    fn admit(&self, id: &SessionId) -> Result<(), String> {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if clients.contains_key(id) {
            return Ok(());
        }
        let named = [&id.target_sub_id, &id.target_location_id, &id.sender_sub_id, &id.sender_location_id];
        let mut named = named.into_iter().flatten().chain([&id.target_comp_id]);
        if named.any(|part| part.contains(['/', ':', '>'])) {
            return Err(format!("session {id}: a CompID, SubID or LocationID with '/', ':' or '>' isn't accepted"));
        }
        let mut restored = self.restored.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = restored.remove(&id.to_string()) {
            clients.insert(id.clone(), client);
            return Ok(());
        }
        // ponytail: a scan of every session, at logon only; a count per CompID if there are many.
        let sessions = clients.keys().filter(|known| known.target_comp_id == id.target_comp_id).count()
            + restored.values().filter(|client| client.comp_id() == id.target_comp_id).count();
        if sessions >= MAX_SESSIONS_PER_COMP_ID {
            return Err(format!("session {id}: its CompID has {sessions} sessions already, the most allowed"));
        }
        clients.insert(id.clone(), ClientId::new(id));
        Ok(())
    }

    /// The client on the acceptor's session `id`. Made at the session's first logon or message
    /// and kept, so a message costs a lookup and a clone rather than formatting the session. Kept
    /// for every session seen since start, as the outbox and the book keep theirs.
    fn client_of(&self, id: &SessionId) -> ClientId {
        // Nothing is left half-changed under this lock, so a panic elsewhere doesn't spoil it.
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        // `admit` (at verify_logon, before any logon or message) has added a live session already,
        // under its per-CompID cap: the insert below is for unit tests that skip the logon.
        if let Some(client) = clients.get(id) {
            return client.clone();
        }
        let client = ClientId::new(id);
        clients.insert(id.clone(), client.clone());
        client
    }

    /// Runs `input` through the book and sends what it answers, all under the lock. A venue
    /// request that can't be queued goes back in as [`Input::SendFailed`], which never makes
    /// venue-bound output, so this ends after one more round per refused request.
    fn handle(self: &Arc<Self>, input: Input) {
        self.handle_all([input]);
    }

    /// As [`handle`](Self::handle), each of `inputs` in turn under one lock.
    fn handle_all(self: &Arc<Self>, inputs: impl IntoIterator<Item = Input>) {
        self.handle_locked(&mut self.lock(), inputs, None);
    }

    /// Handles `input` from `client`, rolling the client's trading day over first if it has ended,
    /// so the input lands in the day it arrived in. `ctx` is the client's message's, held in
    /// flight until each venue request it led to is stored.
    fn handle_from(self: &Arc<Self>, client: &ClientId, input: Input, ctx: &mut Context<'_>) {
        let mut state = self.lock();
        let new_day = new_day(&state, client, self.clock.now()).map(|day| Input::DaysStarted { days: vec![day] });
        self.handle_locked(&mut state, new_day.into_iter().chain([input]), Some(ctx));
    }

    /// One tick's roll: every session whose trading day has ended, quiet ones too, starts its
    /// next, freeing what the book kept for its done orders. One input for all of them, so the
    /// book walks its orders once.
    fn roll_days(self: &Arc<Self>) {
        let mut state = self.lock();
        let now = self.clock.now();
        let state_now = &*state;
        let days: Vec<(ClientId, Day)> = state_now
            .book
            .days()
            .filter(|(_, day)| now >= day.0)
            .filter_map(|(client, _)| new_day(state_now, client, now))
            .collect();
        if !days.is_empty() {
            self.handle_locked(&mut state, [Input::DaysStarted { days }], None);
        }
    }

    /// Handles `input` from a venue's message, `ctx`'s, held until what it changed has committed.
    fn handle_with(self: &Arc<Self>, input: Input, ctx: &mut Context<'_>) {
        self.handle_locked(&mut self.lock(), [input], Some(ctx));
    }

    /// As [`handle_all`](Self::handle_all), under the lock held as `state`. With `ctx`, the
    /// message the inputs came from, which [`send`](Self::send) and [`commit`](Self::commit)
    /// hold.
    fn handle_locked(
        self: &Arc<Self>,
        state: &mut State,
        inputs: impl IntoIterator<Item = Input>,
        mut ctx: Option<&mut Context<'_>>,
    ) {
        // After the lock, so TransactTimes rise in the order the book answers.
        let clock_now = self.clock.now();
        state.book.set_now(clock_now);
        if let Some(writer) = &self.writer {
            state.book.set_unavailable(writer.unavailable());
        }
        let now = UtcTimestamp::from(clock_now);
        let mut inputs = VecDeque::from_iter(inputs);
        let mut outputs = Vec::new();
        let mut reports = Vec::new();
        while let Some(input) = inputs.pop_front() {
            let send_failed = matches!(input, Input::SendFailed { .. });
            let resent = matches!(input, Input::Unsent { .. });
            state.book.handle(input, &mut outputs);
            for output in outputs.drain(..) {
                if let Some(failed) = self.send(state, &output, (now, resent), ctx.as_deref_mut(), &mut reports) {
                    debug_assert!(!send_failed, "SendFailed made venue-bound output");
                    inputs.push_back(failed);
                }
            }
        }
        self.publish(&mut state.book);
        self.commit(&mut state.book, reports, ctx);
        if state.book.take_cancel_unsent() {
            self.cancels_waiting.store(true, Ordering::Release);
        }
    }

    /// Sends what the step just handled changed to the listeners, if any, before
    /// [`commit`](Self::commit) takes the changes: the book's events, then each order it touched.
    /// Sent under the lock, so two steps' events can't pass each other on the way out; a send is
    /// a slot written and listeners woken, and never waits on a listener.
    fn publish(&self, book: &mut Book) {
        if self.events.receiver_count() == 0 {
            book.take_events();
            return;
        }
        let views = book.touched_views();
        #[cfg(test)]
        self.views_built.fetch_add(views.len(), Ordering::Relaxed);
        for event in book.take_events().chain(views.into_iter().map(GatewayEvent::Order)) {
            // An error only once every listener has gone since the check.
            let _ = self.events.send(event);
        }
    }

    /// Every `every`, sends the gateway's cancels waiting for a venue whose session is connected
    /// (those its session refused while up), and starts the next trading day of each client
    /// session whose day has ended. Runs until its task is aborted; [`Service`] starts it with
    /// [`TICK_INTERVAL`].
    ///
    /// [`Service`]: crate::service::Service
    pub async fn run_ticks(self: Arc<Self>, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.send_waiting_cancels();
            self.roll_days();
            // The database falling behind, or catching up, raises its alarm with no input to
            // notice it: a lock a second, with a database only.
            if self.writer.is_some() {
                self.handle_all([]);
            }
        }
    }

    /// One tick's retry of the gateway's cancels: see [`run_ticks`](Self::run_ticks). Takes the
    /// book's lock only after a refused send. A refusal during this one sets the flag again for
    /// the next tick.
    fn send_waiting_cancels(self: &Arc<Self>) {
        if !self.cancels_waiting.swap(false, Ordering::AcqRel) {
            return;
        }
        let venues: BTreeSet<VenueId> = {
            let state = self.lock();
            // ponytail: walks every order after a gateway cancel's send is refused; a per-venue
            // count if it shows.
            state
                .book
                .cancelling()
                .into_iter()
                .filter(|(_, gateway)| gateway.unsent)
                .filter_map(|(id, _)| state.book.order(id).map(|order| order.venue.clone()))
                .filter(|venue| state.venues.get(venue).is_some_and(|to| to.session.is_connected()))
                .collect()
        };
        if !venues.is_empty() {
            self.handle_all(venues.into_iter().map(|venue| Input::VenueUp { venue }));
        }
    }

    /// Sends `output` where it's going, a client's report into `reports` for
    /// [`commit`](Self::commit). `Some` for a venue request its session refused. A venue request
    /// queued holds `ctx`'s message in flight until the venue's session has stored it, so a stop
    /// before then has the client resend it rather than losing it. A venue request `resent`
    /// (see [`Input::Unsent`]) is marked PossResend(97).
    fn send(
        self: &Arc<Self>,
        state: &State,
        output: &Output,
        (now, resent): (UtcTimestamp, bool),
        ctx: Option<&mut Context<'_>>,
        reports: &mut Vec<(ClientId, Message)>,
    ) -> Option<Input> {
        match output {
            Output::Report { client, .. } | Output::CancelReject { client, .. } => {
                let msg = self.client_version.client_message(output, now).expect("a client-bound output");
                // Not `ctx.send`, even to the client whose request this answers: the outbox keeps
                // each client's reports in the book's order. Without a database, the client's
                // request can be committed before its report is stored, so a stop in between
                // loses the report; with one, the report's row outlives the stop.
                reports.push((client.clone(), msg));
                None
            }
            // Only `request_status` asks for one, and sends it itself; never failed back to the
            // book, which recorded nothing to fail.
            Output::StatusRequest { venue, downstream, .. } => {
                debug_assert!(false, "only request_status asks for a status request");
                if let Err(refused) = send_status_request(state.venues.get(venue), output, now) {
                    warn!(%venue, %downstream, "status request not sent: {refused}");
                }
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
                let mut msg = to.version.venue_message(output, now).expect("a venue-bound output");
                if resent {
                    msg.set(tags::POSS_RESEND, true);
                }
                match to.session.send(msg) {
                    Ok(receipt) => {
                        self.settle(receipt, venue.clone(), downstream.clone(), ctx.map(Context::hold));
                        None
                    }
                    Err(SendError::Full(_)) => Some(failed(VENUE_BUSY)),
                    // NotConnected.
                    Err(_) => Some(failed(VENUE_UNAVAILABLE)),
                }
            }
        }
    }

    /// Queues what `book` changed since it was last asked, with `reports`' rows, for the writer,
    /// holding `ctx`'s message until they've committed, then hands the reports to the outbox.
    /// Queued first, under the book's lock: a later change never commits before an earlier one,
    /// and a report's row is queued before the outbox can queue its deletion. Without a writer,
    /// the changes are dropped, so the book's tracking holds nothing.
    ///
    /// Reports made from a message that may have been handled before a restart are marked
    /// PossResend(97): the database may have lost what handling it changed while the reports went
    /// out, and then the same reports, under the same ExecIDs, go again.
    fn commit(&self, book: &mut Book, mut reports: Vec<(ClientId, Message)>, ctx: Option<&mut Context<'_>>) {
        if ctx.as_ref().is_some_and(|ctx| ctx.maybe_redelivered()) {
            for (_, msg) in &mut reports {
                msg.set(tags::POSS_RESEND, true);
            }
        }
        let Some(writer) = &self.writer else {
            book.discard_changes();
            for (client, msg) in reports {
                self.outbox.deliver(&client, msg, None);
            }
            return;
        };
        let mut changes = book.take_changes();
        let begin_string = self.client_version.begin_string();
        let rows: Vec<u64> = reports
            .iter()
            .map(|(client, msg)| {
                let row = writer.next_row();
                // Framed as the client's session would frame it, header aside, so it decodes.
                let framed = msg.clone().with(tags::BEGIN_STRING, begin_string);
                let message = codec::encode(&framed).expect("it has a BeginString");
                changes.undelivered.push((row, Undelivered { client: client.clone(), message }));
                row
            })
            .collect();
        if !changes.is_empty() {
            writer.queue(changes, ctx.map(|ctx| ctx.hold()));
        }
        for ((client, msg), row) in reports.into_iter().zip(rows) {
            if !self.outbox.deliver(&client, msg, Some(row)) {
                writer.delivered(row);
            }
        }
    }

    /// Waits for the venue's session to store a request, and if it drops it unsent, tells the
    /// book, so the client hears rather than waiting on an order the venue never saw. `hold`, the
    /// client's message the request came from, is released once the receipt resolves and, if the
    /// request was dropped, what the book made of that has committed.
    ///
    /// ponytail: one task per venue request; a per-venue drain like the outbox's if it measures.
    fn settle(self: &Arc<Self>, receipt: Receipt, venue: VenueId, downstream: CompactString, hold: Option<Hold>) {
        let gateway = self.clone();
        tokio::spawn(async move {
            // Dropped at the end, after the SendFailed the book handled has committed, not before:
            // a stop in between would leave the order pending with nothing to redeliver it.
            let _hold = hold;
            let dropped = match receipt.await {
                Ok(_) => return,
                Err(dropped @ (Dropped::Disconnected | Dropped::LoggingOut | Dropped::Rejected(_))) => dropped,
                // Storage, or one Turbojet adds: it may have been stored, and then it's resent
                // from there, so the order stays pending rather than be rejected while it works.
                Err(dropped) => {
                    return error!(%venue, %downstream, "venue request may not have been stored: {dropped}");
                }
            };
            let text = format_compact!("{VENUE_UNAVAILABLE}: {dropped}");
            gateway.handle(Input::SendFailed { venue, downstream, text });
            gateway.committed().await;
        });
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|_| {
            // A panic inside the book may have left it half-updated. Going on would be wrong, and
            // so would panicking again: Turbojet answers a panicking callback with a
            // BusinessMessageReject and carries on, so every venue fill would be consumed and
            // lost. Stop the process, and let the venues resend to the next one.
            error!("the book panicked while handling a message; stopping");
            std::process::abort()
        })
    }
}

/// Sends a status request to venue `to`, if its session is connected and takes it. The receipt
/// isn't waited on: a request lost is asked again.
fn send_status_request(to: Option<&Venue>, output: &Output, now: UtcTimestamp) -> Result<(), StatusError> {
    let to = to.filter(|to| to.session.is_connected()).ok_or(StatusError::VenueUnavailable)?;
    let msg = to.version.venue_message(output, now).expect("a venue-bound output");
    to.session.send(msg).map(drop).map_err(|_| StatusError::VenueUnavailable)
}

/// When `client`'s trading day ends, as of `now`: at the next `day_end`, or sooner, when its
/// schedule's period ends (the period containing `now`, or between periods the next one). So a
/// weekly period is several days, each ending at `day_end`, the last when the period does, and a
/// session without a schedule, or whose schedule has no period ahead, ends its day at `day_end`.
/// `None` only if `day_end` has no period at `now`, which its periods, end to end, rule out.
fn day_of(state: &State, client: &ClientId, now: DateTime<Utc>) -> Option<Day> {
    let day_end = state.day_end.period_at(now)?.end;
    let Some(schedule) = (state.schedules.0)(client.comp_id()) else { return Some(Day(day_end)) };
    let period = schedule.period_at(now).or_else(|| schedule.period_at(schedule.next_start(now)?));
    Some(Day(period.map_or(day_end, |period| period.end.min(day_end))))
}

/// `client` and its next trading day, if the one the book has for it has ended or it has none
/// yet. A day is known by its end, so until then there's nothing to look up.
fn new_day(state: &State, client: &ClientId, now: DateTime<Utc>) -> Option<(ClientId, Day)> {
    let current = state.book.day(client);
    if current.is_some_and(|day| now < day.0) {
        return None;
    }
    let day = day_of(state, client, now)?;
    (current != Some(day)).then(|| (client.clone(), day))
}

/// The clients of `snapshot` whose sessions must log on as the clients they were: those with live
/// orders, a block of their own, or reports in `undelivered`. Others (a day, filled notional, a
/// done order's ClOrdIDs) need nothing of a session, so they don't count against their CompID's
/// sessions after a restart.
fn restored_clients<'a>(snapshot: &'a Snapshot, undelivered: impl Iterator<Item = &'a ClientId>) -> BTreeSet<ClientId> {
    let blocked = snapshot.blocked.iter().filter_map(|scope| match scope {
        Scope::Session(client) => Some(client),
        Scope::CompId(_) | Scope::All => None,
    });
    snapshot
        .orders
        .values()
        .filter(|row| !row.order.status.is_done())
        .map(|row| &row.order.client)
        .chain(blocked)
        .chain(undelivered)
        .cloned()
        .collect()
}

/// The acceptor's application: clients, each its own session (see [`ClientId`]).
#[derive(Debug, Clone)]
pub struct ClientApp(pub Arc<Gateway>);

impl Application for ClientApp {
    fn verify_logon(&self, session: &SessionHandle, _logon: &Message, _: &ConnectionInfo) -> Result<(), String> {
        self.0.admit(session.id())
    }

    fn on_logon(&self, session: &SessionHandle) {
        self.0.outbox.logged_on(&self.0.client_of(session.id()), session);
    }

    /// Cancels the session's working orders at the venues, without a block: the client may log
    /// back on and trade. It hears of each order's end through the outbox, when it's back.
    fn on_cancel_on_disconnect(&self, session: &SessionHandle, ended: Disconnect) {
        warn!(%session, ?ended, "cancelling the orders of a client that didn't come back");
        let client = self.0.client_of(session.id());
        self.0.handle(Input::CancelAll { scope: Scope::Session(client) });
    }

    fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let client = self.0.client_of(ctx.session_id());
        let redelivered = ctx.maybe_redelivered() || fix::possibly_resent(msg);
        let mut input = self.0.client_version.client_input(&client, msg, redelivered)?;
        if let Input::NewOrder { redelivered: true, sent, .. } = &mut input {
            *sent = fix::first_sent(msg);
        }
        self.0.handle_from(&client, input, ctx);
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

    /// The requests a stop kept from the venue, and the gateway's cancels that couldn't be sent
    /// to it, go now.
    fn on_logon(&self, session: &SessionHandle) {
        let venue = self.0.lock().venue_names.get(session.id()).map(|(venue, _)| venue.clone());
        let Some(venue) = venue else { return };
        let unsent = self.0.lock().unsent.remove(&venue);
        let unsent = unsent.map(|unsent| self.0.unsent_inputs(&venue, unsent, session)).unwrap_or_default();
        // Sent before anything the book sends now, as they would have been.
        self.0.handle_all(unsent.into_iter().chain([Input::VenueUp { venue: venue.clone() }]));
        self.0.send_orphan_cancels(&venue);
    }

    fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let known = self.0.lock().venue_names.get(ctx.session_id()).cloned();
        // Only if a reload dropped the venue while its session was up.
        let Some((venue, version)) = known else {
            error!(session = %ctx.session_id(), "message from a venue no longer configured; rejected");
            let reason = BusinessRejectReason::ApplicationNotAvailable;
            return Err(MessageReject::Business { reason, text: VENUE_UNAVAILABLE.into() });
        };
        // `None`: a report the book needn't see (a bust, say), logged inside.
        if let Some(input) = version.venue_input(&venue, msg)?
            && !self.0.orphan_answer(&venue, &input)
        {
            self.0.handle_with(input, ctx);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tokio::time::timeout;
    use turbojet::fields::{Code, Decimal};
    use turbojet::message::tags;
    use turbojet::store::{Commit, SentMessages, SessionLog};
    use turbojet::{
        Acceptor, FixMessage, Initiator, InitiatorConfig, MemoryStorage, MsgType, ReconnectPolicy, SessionConfig,
        SessionStorage,
    };
    use turbojet_fix44 as v44;

    use super::*;
    use crate::book::Scope;
    use crate::router::{Rule, Target};
    use crate::store::Store;

    const VENUE: &str = "NASDAQ";

    /// Every order to [`VENUE`].
    fn gateway() -> Arc<Gateway> {
        let rule = Rule { client: None, ex_destination: None, symbol: None, target: Target::Venue(VENUE.into()) };
        Arc::new(Gateway::new(Router::new(vec![rule]), Version::Fix44))
    }

    fn session(begin_string: &str, target: &str) -> SessionHandle {
        SessionHandle::disconnected(SessionId::new(begin_string, "JETBRIDGE", target))
    }

    fn logon(app: &impl Application, session: &SessionHandle) -> Result<(), String> {
        app.verify_logon(session, &Message::new(MsgType::Logon), &ConnectionInfo::new(None, Vec::new()))
    }

    /// Clients' requests are read in the version the gateway was made for: FIX 4.2 requires
    /// HandlInst(21) on an order, which FIX 4.4 dropped.
    #[test]
    fn clients_speak_the_version_the_gateway_was_made_for() {
        let rule = Rule { client: None, ex_destination: None, symbol: None, target: Target::Venue(VENUE.into()) };
        let app = ClientApp(Arc::new(Gateway::new(Router::new(vec![rule]), Version::Fix42)));
        let client = session("FIX.4.2", "FUNDA");
        let reject = app.on_message(&mut Context::new(&client), &order("C1")).unwrap_err();
        assert_eq!(reject, MessageReject::required_tag_missing(tags::HANDL_INST));
    }

    /// FUNDA's session with TargetSubID `sub_id`.
    fn desk(sub_id: &str) -> SessionHandle {
        SessionHandle::disconnected(SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA").with_target_sub_id(sub_id))
    }

    #[test]
    fn a_comp_id_logs_on_with_a_bounded_number_of_sessions() {
        let app = ClientApp(gateway());
        for n in 0..MAX_SESSIONS_PER_COMP_ID {
            assert_eq!(logon(&app, &desk(&format!("DESK{n}"))), Ok(()));
        }
        let refused = logon(&app, &desk("ONE-TOO-MANY")).unwrap_err();
        assert!(refused.contains("sessions"), "{refused}");
        // Known sessions log on again; other CompIDs aren't counted against FUNDA.
        assert_eq!(logon(&app, &desk("DESK0")), Ok(()));
        assert_eq!(logon(&app, &session("FIX.4.4", "FUNDB")), Ok(()));
    }

    /// Each would print as another session: FUNDA's DESK SubID with LocationID 1, FUNDA's DESK
    /// SubID, FUNDA with a qualifier, or a session addressed by another SubID.
    #[test]
    fn a_name_that_prints_as_another_session_is_refused() {
        let app = ClientApp(gateway());
        let id = |comp_id: &str| SessionId::new("FIX.4.4", "JETBRIDGE", comp_id);
        let refused = [
            id("FUNDA").with_target_sub_id("DESK/1"),
            id("FUNDA").with_target_location_id("LDN/2"),
            id("FUNDA/DESK"),
            id("FUNDA:Q"),
            id("FUNDA").with_target_sub_id("DESK:Q"),
            id("T->FUNDA"),
            id("FUNDA").with_sender_sub_id("S->T"),
            id("FUNDA").with_sender_location_id("LDN/2"),
        ];
        for id in refused {
            let refused = logon(&app, &SessionHandle::disconnected(id.clone())).unwrap_err();
            assert!(refused.ends_with("isn't accepted"), "{id}: {refused}");
        }
        assert_eq!(logon(&app, &desk("DESK")), Ok(()));
        assert_eq!(logon(&app, &SessionHandle::disconnected(id("FUNDA").with_sender_sub_id("S"))), Ok(()));
    }

    #[test]
    fn a_venue_logs_on_only_once_the_gateway_knows_it() {
        let gateway = gateway();
        let (app, venue) = (VenueApp(gateway.clone()), session("FIX.4.4", "VENUE"));
        assert!(logon(&app, &venue).is_err());
        assert!(gateway.set_venues([(VENUE.into(), venue.clone())]).is_empty());
        assert_eq!(logon(&app, &venue), Ok(()));
        gateway.set_venues([]);
        assert!(logon(&app, &venue).is_err());
    }

    #[test]
    fn a_venue_with_a_version_the_gateway_cant_convert_is_skipped_and_the_rest_set() {
        let gateway = gateway();
        let (good, bad) = (session("FIX.4.4", "VENUE"), session("FIX.4.3", "OTHER"));
        let skipped = gateway.set_venues([(VENUE.into(), good.clone()), ("OTHER".into(), bad.clone())]);
        assert_eq!(skipped, ["OTHER"]);
        let app = VenueApp(gateway);
        assert_eq!(logon(&app, &good), Ok(()));
        assert!(logon(&app, &bad).is_err());
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
        LoggedOut,
        Message(Message),
    }

    /// A client or venue in the test: reports what it sees, and as a venue that `answers`, acks
    /// and fills every order in full at 10.
    struct Peer {
        events: UnboundedSender<Event>,
        answers: bool,
    }

    impl Application for Peer {
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
                let qty = order.order_qty.expect("orders carry OrderQty");
                for (exec_type, status, last_qty) in
                    [(v44::ExecType::New, v44::OrdStatus::New, 0), (v44::ExecType::Trade, v44::OrdStatus::Filled, 100)]
                {
                    let (last_qty, px) = (Decimal::from(last_qty), Decimal::from(10));
                    let (exec_id, leaves_qty) = (format!("X{last_qty}"), qty - last_qty);
                    let mut report = v44::ExecutionReport::new(
                        "V-1",
                        exec_id,
                        exec_type,
                        status,
                        order.side.clone(),
                        leaves_qty,
                        last_qty,
                        px,
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
        (Arc::new(Peer { events, answers: true }), rx)
    }

    /// A venue that answers nothing itself.
    fn quiet_peer() -> (Arc<Peer>, UnboundedReceiver<Event>) {
        let (events, rx) = unbounded_channel();
        (Arc::new(Peer { events, answers: false }), rx)
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
        serve_with(comp_id, app, Arc::new(MemoryStorage::new())).await
    }

    /// As [`serve`], keeping its sessions in `storage`.
    async fn serve_with(comp_id: &str, app: Arc<dyn Application>, storage: Arc<dyn SessionStorage>) -> String {
        let config = SessionConfig::new("FIX.4.4", comp_id);
        let acceptor = Acceptor::new(config, storage, app).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(acceptor.serve(listener));
        addr
    }

    /// An initiator from `comp_id` to `target` at `addr`, serving `app`, not yet running.
    fn initiator(addr: &str, comp_id: &str, target: &str, app: Arc<dyn Application>) -> Initiator {
        initiator_with(addr, comp_id, target, app, Arc::new(MemoryStorage::new()))
    }

    /// As [`initiator`], keeping its session in `storage`.
    fn initiator_with(
        addr: &str,
        comp_id: &str,
        target: &str,
        app: Arc<dyn Application>,
        storage: Arc<dyn SessionStorage>,
    ) -> Initiator {
        let mut config = InitiatorConfig::new(SessionConfig::new("FIX.4.4", comp_id), target);
        config.reconnect = ReconnectPolicy::fixed(Duration::from_millis(50));
        Initiator::new(addr, config, storage, app).unwrap()
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
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        let (client, mut client_events) = client(&gateway).await;

        client.handle().send(order("C1")).unwrap();
        let sent: v44::NewOrderSingle = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "1");
        let new = report(&mut client_events).await;
        assert_eq!(
            (new.cl_ord_id.as_deref(), new.exec_type, new.ord_status),
            (Some("C1"), Code::Known(v44::ExecType::New), Code::Known(v44::OrdStatus::New))
        );
        let fill = report(&mut client_events).await;
        assert_eq!((fill.cl_ord_id.as_deref(), fill.exec_type), (Some("C1"), Code::Known(v44::ExecType::Trade)));
        assert_eq!((fill.last_qty, fill.cum_qty, fill.leaves_qty), (Some(100.into()), 100.into(), 0.into()));
    }

    /// With the venue's initiator not connected, the order is rejected rather than left pending.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_order_for_a_venue_not_connected_is_rejected() {
        let gateway = gateway();
        let venue = initiator("127.0.0.1:1", "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        let (client, mut events) = client(&gateway).await;

        client.handle().send(order("C1")).unwrap();
        let rejected = report(&mut events).await;
        assert_eq!(
            (rejected.cl_ord_id.as_deref(), rejected.exec_type),
            (Some("C1"), Code::Known(v44::ExecType::Rejected))
        );
        assert_eq!(rejected.text.as_deref(), Some(VENUE_UNAVAILABLE));
    }

    /// A venue's initiator started before the gateway knows it, as `turbojet-config` starts
    /// them: its first logon is refused, and once the gateway is told, the retry gets through.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_venue_started_before_the_gateway_knows_it_logs_on_once_told() {
        let gateway = gateway();
        let (venue_app, mut venue_events) = peer();
        let venue_addr = serve("VENUE", venue_app).await;
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        tokio::spawn(venue.clone().run());
        // The venue's side completes the logon, and then ours refuses it.
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOut));
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        let (client, _client_events) = client(&gateway).await;
        client.handle().send(order("C1")).unwrap();
        let sent: v44::NewOrderSingle = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "1");
    }

    /// A venue that holds our first Logon until released, then refuses it, and every later one.
    struct Refuser {
        holding: UnboundedSender<()>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl Application for Refuser {
        fn verify_logon(&self, _: &SessionHandle, _: &Message, _: &ConnectionInfo) -> Result<(), String> {
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                let _ = self.holding.send(());
                tokio::task::block_in_place(|| release.recv()).ok();
            }
            Err("not today".into())
        }
    }

    /// An order and its cancel, queued for the venue while our logon to it is under way, are
    /// dropped unsent when the logon fails: the client hears of both rather than wait.
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_dropped_unsent_by_the_venue_session_are_answered() {
        let gateway = gateway();
        let (holding, mut held) = unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let refuser = Arc::new(Refuser { holding, release: Mutex::new(Some(gate)) });
        let venue_addr = serve("VENUE", refuser).await;
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        timeout(Duration::from_secs(5), held.recv()).await.expect("logon not held");
        let (client, mut events) = client(&gateway).await;

        client.handle().send(order("C1")).unwrap();
        let mut cancel = v44::OrderCancelRequest::new("C1", "C2", v44::Side::Buy, UtcTimestamp::now());
        cancel.symbol = Some("AAPL".into());
        client.handle().send(cancel.to_message()).unwrap();
        // Neither refused straight away: both are queued on the session.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(events.try_recv().is_err(), "answered before the logon failed");
        release.send(()).unwrap();

        // In either order: each failure is settled on its own task.
        let mut answers = Vec::new();
        for _ in 0..2 {
            answers.push(next_message(&mut events).await);
        }
        answers.sort_by_key(|msg| msg.msg_type().code().to_owned());
        let rejected: v44::ExecutionReport = answers[0].parse().unwrap();
        assert_eq!(
            (rejected.cl_ord_id.as_deref(), rejected.exec_type),
            (Some("C1"), Code::Known(v44::ExecType::Rejected))
        );
        assert!(rejected.text.as_deref().unwrap().starts_with(VENUE_UNAVAILABLE), "{:?}", rejected.text);
        let refused: v44::OrderCancelReject = answers[1].parse().unwrap();
        assert_eq!((refused.cl_ord_id.as_str(), refused.orig_cl_ord_id.as_str()), ("C2", "C1"));
    }

    /// Order 1 in the book, from FUNDA, as if sent to [`VENUE`]: what the book asked to send is
    /// dropped.
    pub(crate) fn book_order(gateway: &Gateway) {
        book_order_as(gateway, "C1");
    }

    /// [`book_order`], the order `cl_ord_id`, booked at the book's time.
    fn book_order_as(gateway: &Gateway, cl_ord_id: &str) {
        let request = crate::book::OrderRequest {
            cl_ord_id: cl_ord_id.into(),
            account: None,
            symbol: "AAPL".into(),
            side: crate::book::Side::Buy,
            ord_type: crate::book::OrdType::Limit,
            qty: 100.into(),
            price: Some(10.into()),
            ex_destination: None,
            time_in_force: None,
        };
        gateway.lock().book.handle(
            Input::NewOrder { client: "FUNDA".into(), request, redelivered: false, sent: None },
            &mut Vec::new(),
        );
    }

    /// [`book_order`], the order `cl_ord_id`, booked now by the wall clock.
    pub(crate) fn book_order_now(gateway: &Gateway, cl_ord_id: &str) {
        gateway.lock().book.set_now(chrono::Utc::now());
        book_order_as(gateway, cl_ord_id);
    }

    /// The venue's acknowledgement of the order sent as `downstream`, naming it `venue_order_id`.
    pub(crate) fn acknowledge(gateway: &Gateway, downstream: &str, venue_order_id: &str) {
        let report = crate::book::VenueReport {
            downstream: downstream.into(),
            exec_type: crate::book::ExecType::New,
            status: Some(crate::book::Status::New),
            last_qty: 0.into(),
            last_px: 0.into(),
            cum_qty: 0.into(),
            avg_px: 0.into(),
            exec_id: "E1".into(),
            text: None,
            venue_order_id: Some(venue_order_id.into()),
            ord_rej_reason: None,
        };
        gateway.lock().book.handle(Input::VenueReport { venue: VENUE.into(), report }, &mut Vec::new());
    }

    /// The venue's answer to a status request for the order sent as `downstream`: it doesn't
    /// know it.
    pub(crate) fn unknown_at_venue(gateway: &Gateway, downstream: &str) {
        let report = crate::book::VenueReport {
            downstream: downstream.into(),
            exec_type: crate::book::ExecType::Status,
            status: Some(crate::book::Status::Rejected),
            last_qty: 0.into(),
            last_px: 0.into(),
            cum_qty: 0.into(),
            avg_px: 0.into(),
            exec_id: "E-unknown".into(),
            text: None,
            venue_order_id: None,
            ord_rej_reason: Some(5),
        };
        gateway.lock().book.handle(Input::VenueReport { venue: VENUE.into(), report }, &mut Vec::new());
    }

    fn unsent(gateway: &Gateway) -> Vec<bool> {
        gateway.cancelling().into_iter().map(|(_, cancel)| cancel.unsent).collect()
    }

    /// The kill switch while the venue is down: the cancel waits, the retry tick leaves it (the
    /// venue isn't connected), and it goes when the venue logs on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_gateway_cancel_for_a_venue_that_is_down_goes_when_it_logs_on() {
        let gateway = gateway();
        let (venue_app, mut venue_events) = peer();
        let venue_addr = serve("VENUE", venue_app).await;
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        book_order(&gateway);
        gateway.cancel_all(Scope::All).await;
        assert!(gateway.cancels_waiting.load(Ordering::Acquire));
        // The venue isn't connected: the tick sends nothing, and leaves it to the logon.
        gateway.send_waiting_cancels();
        assert_eq!(unsent(&gateway), [true]);
        assert!(!gateway.cancels_waiting.load(Ordering::Acquire));

        tokio::spawn(venue.clone().run());
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        let sent: v44::OrderCancelRequest = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), ("1-2", "1"));
        assert_eq!(unsent(&gateway), [false]);
    }

    /// A refused send of a client's order leaves no gateway cancel waiting for the tick.
    #[tokio::test]
    async fn only_a_gateway_cancel_left_unsent_wakes_the_retry_tick() {
        let gateway = gateway();
        book_order(&gateway);
        gateway.handle(Input::SendFailed { venue: VENUE.into(), downstream: "1".into(), text: VENUE_BUSY.into() });
        assert!(!gateway.cancels_waiting.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn block_and_unblock_say_whether_they_changed_anything() {
        let gateway = gateway();
        assert!(gateway.block(Scope::All).await);
        assert!(!gateway.block(Scope::All).await);
        assert!(!gateway.unblock(Scope::CompId("FUNDA".into())).await);
        assert!(gateway.unblock(Scope::All).await);
        assert!(gateway.blocked().is_empty());
    }

    /// A stuck gateway cancel (the venue says it doesn't know the order) starts again with its
    /// tries counted afresh: here it waits, as the venue isn't connected.
    #[tokio::test]
    async fn cancelling_one_order_rearms_a_stuck_cancel() {
        let gateway = gateway();
        book_order(&gateway);
        {
            let book = &mut gateway.lock().book;
            book.handle(Input::CancelOrder { order: OrderId(1) }, &mut Vec::new());
            let refused = Input::VenueCancelReject {
                venue: VENUE.into(),
                downstream: "1-1".into(),
                status: Some(crate::book::Status::New),
                text: None,
                reason: Some(1),
                venue_order_id: None,
            };
            book.handle(refused, &mut Vec::new());
        }
        assert!(gateway.cancelling()[0].1.stuck);
        assert_eq!(gateway.cancel_order(OrderId(1)).await, Ok(()));
        let (_, cancel) = &gateway.cancelling()[0];
        assert_eq!((cancel.tries, cancel.unsent, cancel.stuck), (0, true, false));
    }

    #[tokio::test]
    async fn cancelling_an_unknown_or_done_order_is_refused() {
        let gateway = gateway();
        assert_eq!(gateway.cancel_order(OrderId(1)).await, Err(CancelError::UnknownOrder));
        // No venue set: rejected, so done.
        send_order(&gateway, "C1");
        assert_eq!(gateway.cancel_order(OrderId(1)).await, Err(CancelError::Done));
        assert!(gateway.cancelling().is_empty());
    }

    /// A status request is refused for an unknown or done order, and for a live one whose venue
    /// isn't connected, rather than waiting for it.
    #[tokio::test]
    async fn a_status_request_is_refused_unknown_done_or_with_the_venue_down() {
        let gateway = gateway();
        assert_eq!(gateway.request_status(OrderId(1)), Err(StatusError::UnknownOrder));
        book_order(&gateway);
        assert_eq!(gateway.request_status(OrderId(1)), Err(StatusError::VenueUnavailable));
        let venue = session("FIX.4.4", "VENUE");
        assert!(gateway.set_venues([(VENUE.into(), venue)]).is_empty());
        assert_eq!(gateway.request_status(OrderId(1)), Err(StatusError::VenueUnavailable));
        send_order(&gateway, "C2");
        assert_eq!(gateway.request_status(OrderId(2)), Err(StatusError::Done));
    }

    /// Resolving is refused for an unknown or done order, and for a live one its venue hasn't
    /// said it doesn't know; once it has, the order is canceled.
    #[tokio::test]
    async fn resolving_needs_the_venue_to_have_said_it_doesnt_know_the_order() {
        let gateway = gateway();
        assert_eq!(gateway.resolve(OrderId(1)).await, Err(ResolveError::UnknownOrder));
        book_order(&gateway);
        assert_eq!(gateway.resolve(OrderId(1)).await, Err(ResolveError::NotUnknownAtVenue));
        let downstream = gateway.order(OrderId(1)).unwrap().requests[0].downstream.clone();
        unknown_at_venue(&gateway, &downstream);
        assert_eq!(gateway.resolve(OrderId(1)).await, Ok(()));
        assert_eq!(gateway.order(OrderId(1)).unwrap().order.status, crate::book::Status::Canceled);
        assert_eq!(gateway.resolve(OrderId(1)).await, Err(ResolveError::Done));
    }

    /// A session with a SubID is found by the session as it shows, and names a session scope.
    #[test]
    fn a_client_is_found_by_its_session() {
        let gateway = gateway();
        let desk1 = desk("DESK1");
        assert_eq!(logon(&ClientApp(gateway.clone()), &desk1), Ok(()));
        let shown = desk1.id().to_string();
        assert_eq!(gateway.client(&shown), Some(ClientId::new(desk1.id())));
        assert_eq!(gateway.client(&desk("DESK2").id().to_string()), None);
        let scope = gateway.scope(&format!("session:{shown}").parse().unwrap());
        assert_eq!(scope, Some(Scope::Session(ClientId::new(desk1.id()))));
        assert_eq!(scope.unwrap().to_string(), format!("session:{shown}"));
        assert_eq!(gateway.scope(&"session:FIX.4.4:JETBRIDGE->FUNDB".parse().unwrap()), None);
        assert_eq!(gateway.scope(&"comp:FUNDB".parse().unwrap()), Some(Scope::CompId("FUNDB".into())));
    }

    fn at(time: &str) -> DateTime<Utc> {
        time.parse().unwrap()
    }

    /// A gateway with [`gateway`]'s routes, its days by `schedules` (midnight UTC for a session
    /// without one), and a clock at 2026-10-10 12:00 UTC that the test moves.
    fn gateway_at_noon(schedules: Schedules) -> (Arc<Gateway>, Arc<Mutex<DateTime<Utc>>>) {
        let now = Arc::new(Mutex::new(at("2026-10-10T12:00:00Z")));
        let clock = {
            let now = now.clone();
            Clock::from_fn(move || *now.lock().unwrap())
        };
        let day_end = crate::config::day_end("00:00").unwrap();
        let gateway = Arc::try_unwrap(gateway()).unwrap().with_days(day_end, schedules).with_clock(clock);
        (Arc::new(gateway), now)
    }

    /// When FUNDA's trading day ends, as the book has it.
    fn day_ends(gateway: &Gateway) -> Option<DateTime<Utc>> {
        gateway.lock().book.day(&"FUNDA".into()).map(|day| day.0)
    }

    /// FUNDA sends order `cl_ord_id`; with no venue set, it's booked and rejected, so it's done.
    fn send_order(gateway: &Arc<Gateway>, cl_ord_id: &str) {
        send_message(gateway, "FUNDA", &order(cl_ord_id));
    }

    fn send_message(gateway: &Arc<Gateway>, comp_id: &str, msg: &Message) {
        let client = session("FIX.4.4", comp_id);
        ClientApp(gateway.clone()).on_message(&mut Context::new(&client), msg).unwrap();
    }

    fn set(now: &Mutex<DateTime<Utc>>, time: &str) {
        *now.lock().unwrap() = at(time);
    }

    /// With a schedule, the day ends with its period: the first message after the period ends
    /// starts the next day before it's handled, so a done order's ClOrdID is free again for it.
    /// Between periods, the day ends at the next `day_end`, sooner than the next period does.
    #[tokio::test]
    async fn a_message_after_the_sessions_period_ends_starts_its_next_day() {
        let daily: SessionSchedule = "daily 08:00-17:00".parse().unwrap();
        let (gateway, now) =
            gateway_at_noon(Schedules(Arc::new(move |comp_id| (comp_id == "FUNDA").then(|| daily.clone()))));
        send_order(&gateway, "C1");
        assert_eq!(day_ends(&gateway), Some(at("2026-10-10T17:00:00Z")));
        *now.lock().unwrap() = at("2026-10-10T17:00:00Z");
        send_order(&gateway, "C1");
        assert_eq!(day_ends(&gateway), Some(at("2026-10-11T00:00:00Z")));
        let state = gateway.lock();
        assert!(state.book.order(OrderId(1)).is_none());
        assert_eq!(state.book.order(OrderId(2)).unwrap().request.cl_ord_id, "C1");
    }

    /// A weekly period is a day per `day_end`, the last ending with the period; the weekend
    /// between periods ends at the next `day_end`.
    #[tokio::test]
    async fn a_weekly_period_rolls_at_each_day_end_and_at_its_end() {
        let weekly: SessionSchedule = "weekly sun 17:00-fri 17:00".parse().unwrap();
        let (gateway, now) = gateway_at_noon(Schedules(Arc::new(move |_| Some(weekly.clone()))));
        set(&now, "2026-10-12T12:00:00Z");
        send_order(&gateway, "C1");
        assert_eq!(day_ends(&gateway), Some(at("2026-10-13T00:00:00Z")));
        for (time, ends) in [
            ("2026-10-13T00:00:00Z", "2026-10-14T00:00:00Z"),
            ("2026-10-16T00:00:01Z", "2026-10-16T17:00:00Z"),
            ("2026-10-16T17:00:00Z", "2026-10-17T00:00:00Z"),
            ("2026-10-17T00:00:00Z", "2026-10-18T00:00:00Z"),
        ] {
            set(&now, time);
            gateway.roll_days();
            assert_eq!(day_ends(&gateway), Some(at(ends)), "at {time}");
        }
        assert!(gateway.lock().book.order(OrderId(1)).is_none());
    }

    /// One tick rolls every session whose day has ended.
    #[tokio::test]
    async fn one_tick_rolls_several_sessions() {
        let (gateway, now) = gateway_at_noon(Schedules(Arc::new(|_| None)));
        send_message(&gateway, "FUNDA", &order("C1"));
        send_message(&gateway, "FUNDB", &order("C1"));
        set(&now, "2026-10-11T00:00:00Z");
        gateway.roll_days();
        let state = gateway.lock();
        assert!(state.book.order(OrderId(1)).is_none() && state.book.order(OrderId(2)).is_none());
        for client in ["FUNDA", "FUNDB"] {
            assert_eq!(state.book.day(&client.into()).map(|day| day.0), Some(at("2026-10-12T00:00:00Z")));
        }
    }

    /// A possible resend of an order the book doesn't know, first sent before the session's day
    /// began (by OrigSendingTime), is rejected rather than booked; one first sent since is booked.
    #[tokio::test]
    async fn a_resent_order_from_before_the_day_began_is_rejected() {
        let (gateway, now) = gateway_at_noon(Schedules(Arc::new(|_| None)));
        send_order(&gateway, "C1");
        set(&now, "2026-10-11T00:00:05Z");
        let resent = |cl_ord_id: &str, first_sent: &str| {
            order(cl_ord_id).with(tags::POSS_DUP_FLAG, "Y").with(tags::ORIG_SENDING_TIME, first_sent)
        };
        send_message(&gateway, "FUNDA", &resent("C2", "20261010-23:59:59.000"));
        assert!(gateway.lock().book.order(OrderId(2)).is_none());
        send_message(&gateway, "FUNDA", &resent("C3", "20261011-00:00:00.000"));
        assert_eq!(gateway.lock().book.order(OrderId(2)).unwrap().request.cl_ord_id, "C3");
    }

    /// A session without a schedule ends its day at `day_end`, and the tick rolls it while it's
    /// quiet. A new `day_end` applies from the next day: the one under way ends as it was going to.
    #[tokio::test]
    async fn the_tick_rolls_a_quiet_session_at_day_end_and_a_new_day_end_applies_next() {
        let (gateway, now) = gateway_at_noon(Schedules(Arc::new(|_| None)));
        send_order(&gateway, "C1");
        assert_eq!(day_ends(&gateway), Some(at("2026-10-11T00:00:00Z")));
        gateway.set_day_end(crate::config::day_end("17:00").unwrap());
        *now.lock().unwrap() = at("2026-10-10T23:59:59Z");
        gateway.roll_days();
        assert!(gateway.lock().book.order(OrderId(1)).is_some());
        *now.lock().unwrap() = at("2026-10-11T00:00:00Z");
        gateway.roll_days();
        assert!(gateway.lock().book.order(OrderId(1)).is_none());
        assert_eq!(day_ends(&gateway), Some(at("2026-10-11T17:00:00Z")));
    }

    /// The gateway's venue application, saying when its `on_logon` has run.
    struct LogonSignal(VenueApp, UnboundedSender<()>);

    impl Application for LogonSignal {
        fn verify_logon(&self, session: &SessionHandle, logon: &Message, info: &ConnectionInfo) -> Result<(), String> {
            self.0.verify_logon(session, logon, info)
        }

        fn on_logon(&self, session: &SessionHandle) {
            self.0.on_logon(session);
            let _ = self.1.send(());
        }
    }

    /// The gateway's venue application, saying when its `on_logon` returns when the book had
    /// order 1's request sent (again).
    struct SentOnLogon(VenueApp, UnboundedSender<Option<DateTime<Utc>>>);

    impl Application for SentOnLogon {
        fn verify_logon(&self, session: &SessionHandle, logon: &Message, info: &ConnectionInfo) -> Result<(), String> {
            self.0.verify_logon(session, logon, info)
        }

        fn on_logon(&self, session: &SessionHandle) {
            self.0.on_logon(session);
            let _ = self.1.send(self.0.0.lock().book.sent("1").map(|sent| sent.sent_at));
        }
    }

    /// A request the venue's session never stored is sent again within `on_logon`, before
    /// anything else can reach the venue: a client's cancel arriving just after the logon can't
    /// overtake the order it cancels. Marked PossResend.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unsent_request_goes_again_within_the_venues_logon() {
        let gateway = gateway();
        book_order(&gateway);
        assert_eq!(gateway.lock().book.sent("1").unwrap().sent_at, DateTime::<Utc>::UNIX_EPOCH);
        let unsent = orphans::Unsent { downstreams: vec!["1".into()], next_outgoing: 1 };
        gateway.lock().unsent.insert(VENUE.into(), unsent);
        let (venue_app, mut venue_events) = quiet_peer();
        let venue_addr = serve("VENUE", venue_app).await;
        let (signal, mut logged_on) = unbounded_channel();
        let app = Arc::new(SentOnLogon(VenueApp(gateway.clone()), signal));
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", app);
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        let sent_at = timeout(Duration::from_secs(5), logged_on.recv()).await.expect("no logon").unwrap();
        assert!(sent_at.is_some_and(|at| at > DateTime::<Utc>::UNIX_EPOCH), "not sent within on_logon: {sent_at:?}");
        let sent = next_of_type(&mut venue_events, MsgType::NewOrderSingle).await;
        assert_eq!((sent.get(tags::CL_ORD_ID), sent.get(tags::POSS_RESEND)), (Some("1"), Some("Y")));
    }

    /// A cancel the connected venue's session refused (its queue full) goes on the next tick.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_gateway_cancel_a_connected_venue_refused_goes_on_the_retry_tick() {
        let gateway = gateway();
        let (venue_app, mut venue_events) = peer();
        let venue_addr = serve("VENUE", venue_app).await;
        let (signal, mut logged_on) = unbounded_channel();
        let app = Arc::new(LogonSignal(VenueApp(gateway.clone()), signal));
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", app);
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        // Past the logon's own VenueUp, so only the tick can send the cancel.
        timeout(Duration::from_secs(5), logged_on.recv()).await.expect("no logon");
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        book_order(&gateway);
        // As `send` does when the session's queue is full.
        gateway.lock().book.handle(Input::CancelAll { scope: Scope::All }, &mut Vec::new());
        let text = VENUE_BUSY.into();
        gateway.handle(Input::SendFailed { venue: VENUE.into(), downstream: "1-1".into(), text });
        assert_eq!(unsent(&gateway), [true]);

        tokio::spawn(gateway.clone().run_ticks(Duration::from_millis(10)));
        let sent: v44::OrderCancelRequest = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!((sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_str()), ("1-2", "1"));
        assert_eq!(unsent(&gateway), [false]);
        assert!(!gateway.cancels_waiting.load(Ordering::Acquire));
    }

    /// Storage whose commits wait while the gate is shut: a venue's session slow to store. What a
    /// commit would have written stays in memory until the gate opens and the next commit.
    pub(crate) struct Gated {
        pub(crate) inner: Arc<dyn SessionStorage>,
        pub(crate) open: tokio::sync::watch::Receiver<bool>,
    }

    impl SessionStorage for Gated {
        fn open(&self, id: &SessionId) -> std::io::Result<Box<dyn SessionLog>> {
            Ok(Box::new(GatedLog { inner: self.inner.open(id)?, open: self.open.clone() }))
        }
    }

    struct GatedLog {
        inner: Box<dyn SessionLog>,
        open: tokio::sync::watch::Receiver<bool>,
    }

    impl SessionLog for GatedLog {
        fn next_outgoing(&self) -> u64 {
            self.inner.next_outgoing()
        }

        fn next_incoming(&self) -> u64 {
            self.inner.next_incoming()
        }

        fn set_next_incoming(&mut self, seq: u64) -> std::io::Result<()> {
            self.inner.set_next_incoming(seq)
        }

        fn record_outgoing(&mut self, seq: u64, msg: Option<&[u8]>) -> std::io::Result<()> {
            self.inner.record_outgoing(seq, msg)
        }

        fn sent_messages(&mut self, begin: u64, end: u64) -> std::io::Result<SentMessages> {
            self.inner.sent_messages(begin, end)
        }

        fn reset(&mut self) -> std::io::Result<()> {
            self.inner.reset()
        }

        fn evicted_through(&self) -> Option<u64> {
            self.inner.evicted_through()
        }

        fn in_flight(&self) -> Option<u64> {
            self.inner.in_flight()
        }

        fn set_in_flight(&mut self, seq: u64) -> std::io::Result<()> {
            self.inner.set_in_flight(seq)
        }

        fn set_next_incoming_in_flight(&mut self, seq: u64, in_flight: u64) -> std::io::Result<()> {
            self.inner.set_next_incoming_in_flight(seq, in_flight)
        }

        fn created_at(&self) -> Option<UtcTimestamp> {
            self.inner.created_at()
        }

        fn set_created_at(&mut self, at: UtcTimestamp) -> std::io::Result<()> {
            self.inner.set_created_at(at)
        }

        fn commit(&mut self) -> std::io::Result<Option<Commit>> {
            if *self.open.borrow() {
                return self.inner.commit();
            }
            let mut open = self.open.clone();
            Ok(Some(Commit::future(async move {
                open.wait_for(|open| *open).await.map(drop).map_err(std::io::Error::other)
            })))
        }
    }

    /// A gateway serving FUNDA from `clients`, its venue's session storing only once the gate
    /// opens. The gate's sender, the venue's events and the client's acceptor address.
    async fn gated_venue(
        gateway: &Arc<Gateway>,
        clients: Arc<MemoryStorage>,
    ) -> (tokio::sync::watch::Sender<bool>, UnboundedReceiver<Event>, String) {
        let (venue_app, mut venue_events) = peer();
        let venue_addr = serve("VENUE", venue_app).await;
        let (gate, open) = tokio::sync::watch::channel(true);
        let storage = Arc::new(Gated { inner: Arc::new(MemoryStorage::new()), open });
        let venue = initiator_with(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())), storage);
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        gate.send_replace(false);
        let addr = serve_with("JETBRIDGE", Arc::new(ClientApp(gateway.clone())), clients).await;
        (gate, venue_events, addr)
    }

    /// Waits for `log`'s next incoming number and in-flight marker to be `want`.
    async fn wait_for(log: &dyn SessionLog, want: (u64, Option<u64>)) {
        let reached = async {
            while (log.next_incoming(), log.in_flight()) != want {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let waited = timeout(Duration::from_secs(5), reached).await;
        assert!(waited.is_ok(), "{want:?}, at {:?}", (log.next_incoming(), log.in_flight()));
    }

    /// A client's order stays in flight on its session, recorded as received but with the
    /// store's marker at it, until the venue's session has stored what it was forwarded as.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_clients_order_stays_in_flight_until_its_venue_has_stored_it() {
        let gateway = gateway();
        let clients = Arc::new(MemoryStorage::new());
        let (gate, mut venue_events, addr) = gated_venue(&gateway, clients.clone()).await;
        let (app, mut client_events) = peer();
        let client = initiator(&addr, "FUNDA", "JETBRIDGE", app);
        tokio::spawn(client.clone().run());
        assert!(matches!(next(&mut client_events).await, Event::LoggedOn));
        let log = clients.open(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA")).unwrap();

        client.handle().send(order("C1")).unwrap();
        wait_for(log.as_ref(), (3, Some(2))).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(log.in_flight(), Some(2), "not before the venue stores the order");
        assert!(venue_events.try_recv().is_err(), "sent before it was stored");

        gate.send_replace(true);
        let sent: v44::NewOrderSingle = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "1");
        wait_for(log.as_ref(), (3, Some(3))).await;
        assert_eq!(report(&mut client_events).await.exec_type, v44::ExecType::New);
    }

    /// A client gone before its order was stored at the venue resends it when it's back, as after
    /// a crash: the book knows the ClOrdID and answers with the order's state, so the venue gets
    /// the order once.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_order_resent_after_the_client_reconnects_reaches_the_venue_once() {
        let gateway = gateway();
        let clients = Arc::new(MemoryStorage::new());
        let (gate, mut venue_events, addr) = gated_venue(&gateway, clients.clone()).await;
        let funda = Arc::new(MemoryStorage::new());
        let (app, mut client_events) = peer();
        let client = initiator_with(&addr, "FUNDA", "JETBRIDGE", app.clone(), funda.clone());
        tokio::spawn(client.clone().run());
        assert!(matches!(next(&mut client_events).await, Event::LoggedOn));
        let log = clients.open(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA")).unwrap();
        client.handle().send(order("C1")).unwrap();
        wait_for(log.as_ref(), (3, Some(2))).await;

        client.shutdown(None).await;
        assert!(matches!(next(&mut client_events).await, Event::LoggedOut));
        let client = initiator_with(&addr, "FUNDA", "JETBRIDGE", app, funda);
        tokio::spawn(client.clone().run());
        assert!(matches!(next(&mut client_events).await, Event::LoggedOn));
        let resent = report(&mut client_events).await;
        assert_eq!(
            (resent.cl_ord_id.as_deref(), resent.exec_type, resent.ord_status),
            (Some("C1"), Code::Known(v44::ExecType::OrderStatus), Code::Known(v44::OrdStatus::PendingNew))
        );

        gate.send_replace(true);
        let sent: v44::NewOrderSingle = next_message(&mut venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "1");
        let new = report(&mut client_events).await;
        assert_eq!((new.cl_ord_id.as_deref(), new.exec_type), (Some("C1"), Code::Known(v44::ExecType::New)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let again = std::iter::from_fn(|| venue_events.try_recv().ok())
            .filter(|event| matches!(event, Event::Message(msg) if msg.msg_type() == MsgType::NewOrderSingle))
            .count();
        assert_eq!(again, 0, "the venue got the order again");
    }

    /// [`gateway`], committing to an in-memory database, with new orders rejected once a batch has
    /// waited `max_wait`; and the database, whose writes the test can gate and fail.
    async fn gateway_with_store(max_wait: Duration) -> (Arc<Gateway>, Store) {
        let (gateway, store, _writing) = gateway_with_writer(max_wait).await;
        (gateway, store)
    }

    /// As [`gateway_with_store`], with the writer's task.
    async fn gateway_with_writer(max_wait: Duration) -> (Arc<Gateway>, Store, tokio::task::JoinHandle<()>) {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let (writer, writing) = Writer::start(store.clone(), 1);
        let gateway = Arc::try_unwrap(gateway()).unwrap().with_writer(writer.with_max_wait(max_wait));
        (Arc::new(gateway), store, writing)
    }

    /// A venue (answering if `answers`) and FUNDA, both logged on to `gateway`, each of the
    /// gateway's sessions kept in a storage the test reads: the venue's initiator, the client and
    /// their logs.
    async fn connected(gateway: &Arc<Gateway>, answers: bool) -> Connected {
        let (venue_app, mut venue_events) = if answers { peer() } else { quiet_peer() };
        let venue_addr = serve("VENUE", venue_app).await;
        let venues = Arc::new(MemoryStorage::new());
        let app = Arc::new(VenueApp(gateway.clone()));
        let venue = initiator_with(&venue_addr, "JETBRIDGE", "VENUE", app, venues.clone());
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        assert!(matches!(next(&mut venue_events).await, Event::LoggedOn));
        let clients = Arc::new(MemoryStorage::new());
        let addr = serve_with("JETBRIDGE", Arc::new(ClientApp(gateway.clone())), clients.clone()).await;
        let (app, mut client_events) = peer();
        let client = initiator(&addr, "FUNDA", "JETBRIDGE", app);
        tokio::spawn(client.clone().run());
        assert!(matches!(next(&mut client_events).await, Event::LoggedOn));
        Connected {
            venue_log: venues.open(&SessionId::new("FIX.4.4", "JETBRIDGE", "VENUE")).unwrap(),
            client_log: clients.open(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA")).unwrap(),
            venue,
            venue_events,
            client,
            client_events,
        }
    }

    struct Connected {
        venue: Initiator,
        venue_events: UnboundedReceiver<Event>,
        venue_log: Box<dyn SessionLog>,
        client: Initiator,
        client_events: UnboundedReceiver<Event>,
        client_log: Box<dyn SessionLog>,
    }

    fn uncommitted(gateway: &Gateway) -> usize {
        gateway.writer.as_ref().expect("a gateway with a database").uncommitted()
    }

    /// Waits until every report the outbox took is stored or lost, and its row's deletion
    /// committed.
    async fn delivered(gateway: &Gateway) {
        let settled = async {
            while !gateway.outbox.held_by_client().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(Duration::from_secs(5), settled).await.expect("reports still held");
        timeout(Duration::from_secs(5), gateway.committed()).await.expect("not committed");
    }

    /// With a database, a client's order and the venue's ack and fill each stay in flight on
    /// their session until the batch with what they changed has committed, though the order goes
    /// to the venue and the reports to the client without waiting for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn messages_stay_in_flight_until_their_changes_commit() {
        let (gateway, store) = gateway_with_store(writer::MAX_COMMIT_WAIT).await;
        let mut sessions = connected(&gateway, true).await;
        store.hooks.gate.send_replace(false);

        sessions.client.handle().send(order("C1")).unwrap();
        let sent: v44::NewOrderSingle = next_message(&mut sessions.venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "1");
        assert_eq!(report(&mut sessions.client_events).await.exec_type, v44::ExecType::New);
        assert_eq!(report(&mut sessions.client_events).await.exec_type, v44::ExecType::Trade);
        // The order is 2 on the client's session; the ack and fill 2 and 3 on the venue's.
        wait_for(sessions.client_log.as_ref(), (3, Some(2))).await;
        wait_for(sessions.venue_log.as_ref(), (4, Some(2))).await;
        assert!(store.load().await.unwrap().orders.is_empty(), "committed while the gate was shut");

        store.hooks.gate.send_replace(true);
        wait_for(sessions.client_log.as_ref(), (3, Some(3))).await;
        wait_for(sessions.venue_log.as_ref(), (4, Some(4))).await;
        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.orders[&OrderId(1)].order.status, crate::book::Status::Filled);
    }

    /// A database that fails holds every message that changed the book, and once it's back each
    /// change is written: the store then holds what the book does, and nothing undelivered. Some
    /// writes fail before starting, some after their statements, rolled back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_database_stalls_then_writes_every_change() {
        let (gateway, store) = gateway_with_store(writer::MAX_COMMIT_WAIT).await;
        let mut sessions = connected(&gateway, true).await;
        store.hooks.gate.send_replace(false);
        store.hooks.failures.store(3, Ordering::Release);
        store.hooks.failures_in_transaction.store(2, Ordering::Release);

        for cl_ord_id in ["C1", "C2", "C3"] {
            sessions.client.handle().send(order(cl_ord_id)).unwrap();
        }
        for _ in 0..6 {
            report(&mut sessions.client_events).await;
        }
        wait_for(sessions.client_log.as_ref(), (5, Some(2))).await;
        wait_for(sessions.venue_log.as_ref(), (8, Some(2))).await;

        store.hooks.gate.send_replace(true);
        wait_for(sessions.client_log.as_ref(), (5, Some(5))).await;
        wait_for(sessions.venue_log.as_ref(), (8, Some(8))).await;
        assert_eq!(store.hooks.failures.load(Ordering::Acquire), 0, "every failure was retried");
        assert_eq!(store.hooks.failures_in_transaction.load(Ordering::Acquire), 0);
        delivered(&gateway).await;
        assert_eq!(uncommitted(&gateway), 0);
        let loaded = store.load().await.unwrap();
        assert!(loaded.undelivered.is_empty(), "{:?}", loaded.undelivered);
        assert_eq!(loaded, gateway.lock().book.snapshot());
        assert_eq!(loaded.orders.len(), 3);
    }

    /// A report from the venue for our order `downstream`: an ack, or a fill of `last_qty`.
    fn venue_report(downstream: &str, last_qty: i64) -> Message {
        let (exec_type, status) = if last_qty == 0 {
            (v44::ExecType::New, v44::OrdStatus::New)
        } else {
            (v44::ExecType::Trade, v44::OrdStatus::PartiallyFilled)
        };
        let (last_qty, px) = (Decimal::from(last_qty), Decimal::from(10));
        let exec_id = format!("{downstream}-{last_qty}");
        let mut report = v44::ExecutionReport::new(
            "V-1",
            exec_id,
            exec_type,
            status,
            v44::Side::Buy,
            Decimal::from(100) - last_qty,
            last_qty,
            px,
        );
        report.cl_ord_id = Some(downstream.into());
        (report.last_qty, report.last_px) = (Some(last_qty), Some(px));
        report.to_message()
    }

    /// The reports queued for clients and not yet delivered, as the store has them.
    async fn undelivered(store: &Store) -> Vec<Message> {
        let rows = store.load().await.unwrap().undelivered;
        rows.into_values()
            .map(|row| match codec::decode(&row.message) {
                codec::Decoded::Message(msg, _) => msg,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// Once a batch has waited too long to commit, a new order is rejected as "gateway
    /// unavailable", and isn't booked; a cancel goes to the venue and a fill is booked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_waiting_too_long_rejects_new_orders_and_the_rest_flow() {
        let (gateway, store) = gateway_with_store(Duration::from_millis(50)).await;
        let mut sessions = connected(&gateway, false).await;
        let venue = VenueApp(gateway.clone());
        let venue_message = |msg: &Message| venue.on_message(&mut Context::new(&sessions.venue.handle()), msg).unwrap();
        // From FUNDA's session, though not through it: nothing is held.
        send_order(&gateway, "C1");
        let _: v44::NewOrderSingle = next_message(&mut sessions.venue_events).await.parse().unwrap();
        venue_message(&venue_report("1", 0));
        gateway.committed().await;

        store.hooks.gate.send_replace(false);
        send_order(&gateway, "C2");
        let _: v44::NewOrderSingle = next_message(&mut sessions.venue_events).await.parse().unwrap();
        let writer = gateway.writer.as_ref().unwrap();
        let waited = async {
            while !writer.unavailable() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(Duration::from_secs(5), waited).await.expect("never unavailable");
        send_order(&gateway, "C3");
        assert!(gateway.lock().book.order(OrderId(3)).is_none());
        let mut cancel = v44::OrderCancelRequest::new("C1", "C4", v44::Side::Buy, UtcTimestamp::now());
        cancel.symbol = Some("AAPL".into());
        send_message(&gateway, "FUNDA", &cancel.to_message());
        let sent: v44::OrderCancelRequest = next_message(&mut sessions.venue_events).await.parse().unwrap();
        assert_eq!(sent.orig_cl_ord_id, "1");
        venue_message(&venue_report("1", 10));
        assert_eq!(gateway.lock().book.order(OrderId(1)).unwrap().cum_qty, 10.into());

        store.hooks.gate.send_replace(true);
        timeout(Duration::from_secs(5), gateway.committed()).await.expect("not committed");
        assert_eq!(store.load().await.unwrap().orders[&OrderId(1)].order.cum_qty, 10.into());
        let rejected = loop {
            let report = report(&mut sessions.client_events).await;
            if report.cl_ord_id.as_deref() == Some("C3") {
                break report;
            }
        };
        assert_eq!(rejected.exec_type, v44::ExecType::Rejected);
        assert_eq!(rejected.text.as_deref(), Some(crate::book::GATEWAY_UNAVAILABLE));
    }

    /// A report for a client that isn't logged on is a row once its batch commits, framed as the
    /// client's session would send it; the row goes once the client is back and its session has
    /// stored the report.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reports_row_goes_once_the_clients_session_stores_it() {
        let (gateway, store) = gateway_with_store(writer::MAX_COMMIT_WAIT).await;
        // No venue: booked and rejected.
        send_order(&gateway, "C1");
        gateway.committed().await;
        let rows = store.load().await.unwrap().undelivered;
        assert_eq!(
            rows.values().map(|row| &row.client).collect::<Vec<_>>(),
            [&ClientId::new(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA"))]
        );
        let [msg] = undelivered(&store).await.try_into().unwrap();
        assert_eq!(msg.get(tags::BEGIN_STRING), Some("FIX.4.4"));
        let rejected: v44::ExecutionReport = msg.parse().unwrap();
        assert_eq!(
            (rejected.cl_ord_id.as_deref(), rejected.exec_type),
            (Some("C1"), Code::Known(v44::ExecType::Rejected))
        );

        let (_client, mut events) = client(&gateway).await;
        assert_eq!(report(&mut events).await.cl_ord_id.as_deref(), Some("C1"));
        delivered(&gateway).await;
        assert!(undelivered(&store).await.is_empty());
    }

    /// An operator's call returns only once its change has committed.
    #[tokio::test]
    async fn an_operator_call_returns_once_its_change_has_committed() {
        let (gateway, store) = gateway_with_store(writer::MAX_COMMIT_WAIT).await;
        store.hooks.gate.send_replace(false);
        let mut blocking = tokio::spawn({
            let gateway = gateway.clone();
            async move { gateway.block(Scope::All).await }
        });
        assert!(timeout(Duration::from_millis(100), &mut blocking).await.is_err(), "returned before committing");
        assert!(store.load().await.unwrap().blocked.is_empty());
        store.hooks.gate.send_replace(true);
        timeout(Duration::from_secs(5), blocking).await.expect("not committed").unwrap();
        assert_eq!(store.load().await.unwrap().blocked, [Scope::All]);
    }

    /// Without a database, the book keeps none of its changes between inputs.
    #[tokio::test]
    async fn without_a_database_the_book_keeps_no_changes() {
        let gateway = gateway();
        send_order(&gateway, "C1");
        gateway.cancel_all(Scope::All).await;
        assert!(gateway.unblock(Scope::All).await);
        assert_eq!(gateway.lock().book.take_changes(), crate::book::Changes::default());
    }

    /// A request the venue's session drops unsent keeps the client's order in flight until the
    /// reject the book made of it has committed: a stop before then has the order redelivered,
    /// rather than leave it pending with nothing to answer it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_requests_order_stays_in_flight_until_its_reject_commits() {
        let (gateway, store) = gateway_with_store(writer::MAX_COMMIT_WAIT).await;
        let (holding, mut held) = unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let refuser = Arc::new(Refuser { holding, release: Mutex::new(Some(gate)) });
        let venue_addr = serve("VENUE", refuser).await;
        let venue = initiator(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())));
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        timeout(Duration::from_secs(5), held.recv()).await.expect("logon not held");
        let clients = Arc::new(MemoryStorage::new());
        let addr = serve_with("JETBRIDGE", Arc::new(ClientApp(gateway.clone())), clients.clone()).await;
        let (app, mut events) = peer();
        let client = initiator(&addr, "FUNDA", "JETBRIDGE", app);
        tokio::spawn(client.clone().run());
        assert!(matches!(next(&mut events).await, Event::LoggedOn));
        let log = clients.open(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA")).unwrap();

        client.handle().send(order("C1")).unwrap();
        wait_for(log.as_ref(), (3, Some(2))).await;
        // The order's own batch has committed; the venue's receipt still holds it.
        timeout(Duration::from_secs(5), gateway.committed()).await.expect("not committed");
        assert_eq!(log.in_flight(), Some(2));
        store.hooks.gate.send_replace(false);
        release.send(()).unwrap();
        let rejected = report(&mut events).await;
        assert_eq!(
            (rejected.cl_ord_id.as_deref(), rejected.exec_type),
            (Some("C1"), Code::Known(v44::ExecType::Rejected))
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(log.in_flight(), Some(2), "released before the reject committed");
        store.hooks.gate.send_replace(true);
        wait_for(log.as_ref(), (3, Some(3))).await;
    }

    /// Once the writer's task is cancelled, as when the service is dropped, a message that
    /// changes the book is never released: its hold is forgotten, so it's redelivered after a
    /// restart, and nothing waiting for a commit is told it happened.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_writer_releases_nothing() {
        let (gateway, _store, writing) = gateway_with_writer(writer::MAX_COMMIT_WAIT).await;
        let mut sessions = connected(&gateway, true).await;
        writing.abort();
        assert!(writing.await.unwrap_err().is_cancelled());

        sessions.client.handle().send(order("C1")).unwrap();
        assert_eq!(report(&mut sessions.client_events).await.exec_type, v44::ExecType::New);
        wait_for(sessions.client_log.as_ref(), (3, Some(2))).await;
        assert!(timeout(Duration::from_millis(100), gateway.committed()).await.is_err());
        assert_eq!(sessions.client_log.in_flight(), Some(2), "released unwritten");
    }

    /// A report the outbox refuses (FUNDA's holds one, and has one) has its row's deletion queued
    /// at once, inside `commit`: after the batch inserting the row, or the insert would win and
    /// the row stay for a restart to send.
    #[tokio::test]
    async fn a_report_the_outbox_refuses_leaves_no_row() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let (writer, _writing) = Writer::start(store.clone(), 1);
        let mut gateway = Arc::try_unwrap(gateway()).unwrap();
        gateway.outbox = Outbox::with_limit(1);
        let gateway = Arc::new(gateway.with_writer(writer));
        // No venue: each is booked and rejected, and FUNDA isn't logged on.
        send_order(&gateway, "C1");
        send_order(&gateway, "C2");
        timeout(Duration::from_secs(5), gateway.committed()).await.expect("not committed");
        let rows = store.load().await.unwrap().undelivered;
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), [1], "only the report the outbox holds");
    }

    /// A gateway as a start makes it: [`gateway`]'s routes under `prefix`, committing to `store`
    /// and restored from it.
    async fn restarted(store: &Store, prefix: &str) -> Arc<Gateway> {
        let snapshot = store.load().await.unwrap();
        let (writer, _writing) = Writer::start(store.clone(), snapshot.next_undelivered_id());
        let gateway = Arc::try_unwrap(gateway()).unwrap().with_id_prefix(prefix.into()).with_writer(writer);
        let gateway = Arc::new(gateway);
        gateway.restore(snapshot);
        gateway
    }

    /// What `gateway`'s book answers `input` with, handled directly: nothing sent or committed.
    pub(crate) fn book_input(gateway: &Gateway, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        gateway.lock().book.handle(input, &mut out);
        out
    }

    /// An order `cl_ord_id` for 100 AAPL at 10 from `comp_id`'s session.
    pub(crate) fn new_order_from(comp_id: &str, cl_ord_id: &str) -> Input {
        let request = crate::book::OrderRequest {
            cl_ord_id: cl_ord_id.into(),
            account: None,
            symbol: "AAPL".into(),
            side: crate::book::Side::Buy,
            ord_type: crate::book::OrdType::Limit,
            qty: 100.into(),
            price: Some(10.into()),
            ex_destination: None,
            time_in_force: None,
        };
        Input::NewOrder { client: comp_id.into(), request, redelivered: false, sent: None }
    }

    fn rejected_for(out: &[Output]) -> Option<crate::book::RejectReason> {
        let [Output::Report { report, .. }] = out else { panic!("{out:?}") };
        report.reject
    }

    /// A gateway restarted on its database has what the last committed: FUNDA's live order,
    /// partly filled, answers a status request by its name; its ClOrdID is still today's, so a
    /// duplicate; credit counts what FUNDA filled, as well as the order's open notional (1,000
    /// open and filled, so another 1,000 is over 1,950); FUNDB's block holds. A new order takes
    /// the new prefix.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restarted_gateway_has_what_its_database_committed() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let limits = risk::Config {
            defaults: risk::Limits { credit: Some(1_950.into()), ..risk::Limits::default() },
            ..risk::Config::default()
        };
        let before = restarted(&store, "OLDOLD").await;
        before.set_config(before.router(), limits.clone());
        let mut sessions = connected(&before, false).await;
        send_order(&before, "C1");
        let sent: v44::NewOrderSingle = next_message(&mut sessions.venue_events).await.parse().unwrap();
        assert_eq!(sent.cl_ord_id, "OLDOLD1");
        let venue = VenueApp(before.clone());
        for msg in [venue_report("OLDOLD1", 0), venue_report("OLDOLD1", 10)] {
            venue.on_message(&mut Context::new(&sessions.venue.handle()), &msg).unwrap();
        }
        before.block(Scope::CompId("FUNDB".into())).await;
        before.committed().await;

        let after = restarted(&store, "NEWNEW").await;
        after.set_config(after.router(), limits);
        assert_eq!(after.lock().book.snapshot(), before.lock().book.snapshot());
        let status = Input::StatusRequest {
            client: "FUNDA".into(),
            cl_ord_id: "C1".into(),
            side: crate::book::Side::Buy,
            symbol: "AAPL".into(),
        };
        let out = book_input(&after, status);
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.order_id.as_deref(), report.cum_qty), (Some("OLDOLD1"), 10.into()));
        assert_eq!(
            rejected_for(&book_input(&after, new_order_from("FUNDA", "C1"))),
            Some(crate::book::RejectReason::Duplicate)
        );
        assert_eq!(
            rejected_for(&book_input(&after, new_order_from("FUNDA", "C2"))),
            Some(crate::book::RejectReason::Risk(risk::RiskReject::Credit))
        );
        assert_eq!(
            rejected_for(&book_input(&after, new_order_from("FUNDB", "B1"))),
            Some(crate::book::RejectReason::Blocked)
        );
        let out = book_input(&after, new_order_from("FUNDC", "D1"));
        assert!(matches!(out.as_slice(), [Output::NewOrder { downstream, .. }] if downstream == "NEWNEW2"), "{out:?}");
    }

    /// Sessions the database had that must keep their identity (here, by a block of their own)
    /// count toward their CompID's [`MAX_SESSIONS_PER_COMP_ID`] before they log on, and one
    /// logging on is the client it was, counted once; a session with only a day, or only done
    /// orders, doesn't count.
    #[tokio::test]
    async fn restored_clients_count_toward_their_comp_ids_sessions() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let client = |sub_id: &str| ClientId::new(desk(sub_id).id());
        let today = crate::book::Today { day: Day(at("2026-10-11T00:00:00Z")), began: None };
        let blocked = (0..MAX_SESSIONS_PER_COMP_ID - 1).map(|n| Scope::Session(client(&format!("DESK{n}")))).collect();
        let changes = crate::book::Changes {
            blocked: Some(blocked),
            days: vec![(client("DAY-ONLY"), today)],
            ..Default::default()
        };
        store.write(&[changes]).await.unwrap();
        // DONE's only order is done (rejected, no venue), and its report delivered.
        let before = restarted(&store, "OLDOLD").await;
        ClientApp(before.clone()).on_message(&mut Context::new(&desk("DONE")), &order("D1")).unwrap();
        before.committed().await;
        let rows = store.load().await.unwrap().undelivered.into_keys().collect();
        store.write(&[crate::book::Changes { delivered: rows, ..Default::default() }]).await.unwrap();
        assert!(!store.load().await.unwrap().orders.is_empty());
        drop(before);
        let gateway = restarted(&store, "NEWNEW").await;
        let app = ClientApp(gateway.clone());
        // Known before it logs on, so an operator can name it.
        assert_eq!(gateway.client(&desk("DESK5").id().to_string()), Some(client("DESK5")));
        assert_eq!(logon(&app, &desk("DESK3")), Ok(()));
        assert_eq!(gateway.client_of(desk("DESK3").id()), client("DESK3"));
        // The 16th: DESK3 is counted once, and DAY-ONLY and DONE not at all.
        assert_eq!(logon(&app, &desk("NEW")), Ok(()));
        assert!(logon(&app, &desk("ONE-TOO-MANY")).is_err());
        assert!(logon(&app, &desk("DAY-ONLY")).is_err());
        assert_eq!(logon(&app, &desk("DESK0")), Ok(()));
        assert_eq!(logon(&app, &session("FIX.4.4", "FUNDB")), Ok(()));
    }

    /// A restored client is known by its session's printed name: a CompID printing as it (FUNDA's
    /// DESK as `FUNDA/DESK`) is refused, not given the client, which FUNDA's DESK still gets.
    #[tokio::test]
    async fn a_restored_client_isnt_taken_by_a_name_printing_as_its() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let funda_desk = ClientId::new(desk("DESK").id());
        let changes =
            crate::book::Changes { blocked: Some(vec![Scope::Session(funda_desk.clone())]), ..Default::default() };
        store.write(&[changes]).await.unwrap();
        let gateway = restarted(&store, "NEWNEW").await;
        let app = ClientApp(gateway.clone());
        let lookalike = SessionHandle::disconnected(SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA/DESK"));
        assert_eq!(lookalike.id().to_string(), funda_desk.to_string());
        assert!(logon(&app, &lookalike).is_err());
        assert_eq!(logon(&app, &desk("DESK")), Ok(()));
        assert_eq!(gateway.client_of(desk("DESK").id()), funda_desk);
    }

    /// At restore, a stored report that doesn't decode has its row deleted, as has one past a
    /// full outbox; the rest wait for their client.
    #[tokio::test]
    async fn undelivered_rows_that_cant_be_queued_are_deleted() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let funda = ClientId::from("FUNDA");
        let report = codec::encode(&Message::new(MsgType::Heartbeat).with(tags::BEGIN_STRING, "FIX.4.4")).unwrap();
        let undelivered = [vec![0xff, 0, 1], report.clone(), report]
            .into_iter()
            .zip(1..)
            .map(|(message, row)| (row, Undelivered { client: funda.clone(), message }))
            .collect();
        store.write(&[crate::book::Changes { undelivered, ..Default::default() }]).await.unwrap();
        let snapshot = store.load().await.unwrap();
        let (writer, _writing) = Writer::start(store.clone(), snapshot.next_undelivered_id());
        let mut gateway = Arc::try_unwrap(gateway()).unwrap();
        gateway.outbox = Outbox::with_limit(1);
        let gateway = Arc::new(gateway.with_writer(writer));
        gateway.restore(snapshot);
        gateway.committed().await;
        let rows = store.load().await.unwrap().undelivered;
        assert_eq!(rows.keys().copied().collect::<Vec<_>>(), [2]);
    }

    /// A report the database holds undelivered goes to its client once it logs on to the
    /// restarted gateway, marked PossResend, once; and its row goes.
    #[tokio::test(flavor = "multi_thread")]
    async fn undelivered_reports_go_again_marked_possibly_resent_and_their_rows_go() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let before = restarted(&store, "OLDOLD").await;
        // No venue: booked and rejected, and FUNDA isn't logged on.
        send_order(&before, "C1");
        before.committed().await;
        assert_eq!(store.load().await.unwrap().undelivered.len(), 1);
        drop(before);

        let after = restarted(&store, "NEWNEW").await;
        let (_client, mut events) = client(&after).await;
        let resent = next_message(&mut events).await;
        assert_eq!(resent.get(tags::POSS_RESEND), Some("Y"));
        let rejected: v44::ExecutionReport = resent.parse().unwrap();
        assert_eq!(
            (rejected.cl_ord_id.as_deref(), rejected.exec_type),
            (Some("C1"), Code::Known(v44::ExecType::Rejected))
        );
        delivered(&after).await;
        assert!(store.load().await.unwrap().undelivered.is_empty());
        assert!(timeout(Duration::from_millis(100), events.recv()).await.is_err(), "sent twice");
    }

    /// `messages` as `id`'s session sent them, in `storage`, sent now unless they say when.
    fn store_sent(storage: &MemoryStorage, id: &SessionId, messages: &[Message]) {
        let mut log = storage.open(id).unwrap();
        // As a session records at its first logon.
        log.set_created_at(UtcTimestamp::from(DateTime::<Utc>::UNIX_EPOCH)).unwrap();
        for (seq, msg) in (1..).zip(messages) {
            let sending_time =
                msg.get(tags::SENDING_TIME).map_or_else(|| UtcTimestamp::now().to_string(), str::to_owned);
            let msg = msg
                .clone()
                .with(tags::BEGIN_STRING, "FIX.4.4")
                .with(tags::MSG_SEQ_NUM, seq)
                .with(tags::SENDER_COMP_ID, id.sender_comp_id.as_str())
                .with(tags::TARGET_COMP_ID, id.target_comp_id.as_str())
                .with(tags::SENDING_TIME, sending_time);
            log.record_outgoing(seq, Some(&codec::encode(&msg).unwrap())).unwrap();
        }
    }

    /// A venue as VENUE, answering nothing, expecting `next_incoming` from us: what our session
    /// stored before is as if it had seen it.
    async fn venue_expecting(next_incoming: u64) -> (String, UnboundedReceiver<Event>) {
        let storage = Arc::new(MemoryStorage::new());
        let mut log = storage.open(&SessionId::new("FIX.4.4", "VENUE", "JETBRIDGE")).unwrap();
        log.set_next_incoming(next_incoming).unwrap();
        let (app, events) = quiet_peer();
        (serve_with("VENUE", app, storage).await, events)
    }

    /// Starts `gateway`'s venue on `storage`, logging on to `addr`.
    fn start_venue(gateway: &Arc<Gateway>, addr: &str, storage: Arc<MemoryStorage>) {
        let venue = initiator_with(addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())), storage);
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.run());
    }

    async fn next_of_type(events: &mut UnboundedReceiver<Event>, msg_type: MsgType) -> Message {
        loop {
            if let Event::Message(msg) = next(events).await
                && msg.msg_type() == msg_type
            {
                return msg;
            }
        }
    }

    /// An order the venue's session stored that the database never had is cancelled at the venue
    /// once it logs on, from the stored order's fields. One the database has, and one under an ID
    /// of another shape, aren't.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_orphaned_order_in_a_venues_store_is_cancelled_when_it_logs_on() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let known = crate::book::Sent {
            order: OrderId(2),
            origin: crate::book::Origin::Client,
            cl_ord_id: "C2".into(),
            orig_cl_ord_id: None,
            kind: None,
            answered: false,
            sent_at: chrono::DateTime::UNIX_EPOCH,
        };
        store
            .write(&[crate::book::Changes { sent: vec![("OLDOLD2".into(), known)], ..Default::default() }])
            .await
            .unwrap();
        let gateway = restarted(&store, "NEWNEW").await;
        let venues = Arc::new(MemoryStorage::new());
        let orphan = order("OLDOLD1").with(tags::ACCOUNT, "ACC");
        store_sent(&venues, &id(), &[orphan, order("OLDOLD2"), order("FOREIGN1")]);

        let found = gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), venues.clone())]).await.unwrap();
        assert_eq!(found, Orphans { scanned: 3, orders: 1, amendments: 0, unsent: 0, pruned: 0 });
        let (addr, mut events) = venue_expecting(4).await;
        let venue_store: Arc<dyn SessionStorage> = venues.clone();
        start_venue(&gateway, &addr, venues);
        let cancel: v44::OrderCancelRequest =
            next_of_type(&mut events, MsgType::OrderCancelRequest).await.parse().unwrap();
        assert_eq!((cancel.orig_cl_ord_id.as_str(), cancel.cl_ord_id.as_str()), ("OLDOLD1", "NEWNEWC1"));
        assert_eq!((cancel.account.as_deref(), cancel.symbol.as_deref()), (Some("ACC"), Some("AAPL")));
        assert_eq!((cancel.side, cancel.order_qty), (Code::Known(v44::Side::Buy), Some(100.into())));
        assert!(gateway.lock().orphan_cancels.is_empty());
        assert!(
            timeout(Duration::from_millis(100), next_of_type(&mut events, MsgType::OrderCancelRequest)).await.is_err()
        );
        // The venue's answers on the orphan are logged, not the book's.
        let answer = |downstream: &str| Input::VenueCancelReject {
            venue: VENUE.into(),
            downstream: downstream.into(),
            status: None,
            text: None,
            reason: Some(1),
            venue_order_id: None,
        };
        assert!(gateway.orphan_answer(&VENUE.into(), &answer("NEWNEWC1")));
        assert!(gateway.orphan_answer(&VENUE.into(), &answer("OLDOLD1")));
        assert!(!gateway.orphan_answer(&VENUE.into(), &answer("OLDOLD2")));

        // Recorded once the session has stored it: the next start doesn't cancel it again.
        let recorded = async {
            loop {
                gateway.committed().await;
                if gateway.lock().orphan_cancels_sending == 0
                    && store.unknown_downstreams(&["OLDOLD1".into()]).await.unwrap().is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(Duration::from_secs(5), recorded).await.expect("never recorded");
        assert!(!gateway.orphans_pending());
        let restarted = restarted(&store, "NEWER1").await;
        let again = restarted.cancel_orphans(&store, vec![(VENUE.into(), id(), venue_store)]).await.unwrap();
        assert_eq!((again.orders, again.amendments), (0, 0));
    }

    /// An order and its replace both orphaned are one order to cancel, naming the replace, the
    /// newest request, with its quantity.
    #[tokio::test]
    async fn an_orphaned_order_and_its_replace_are_cancelled_as_one() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let gateway = restarted(&store, "NEWNEW").await;
        let venues = Arc::new(MemoryStorage::new());
        let mut replace = v44::OrderCancelReplaceRequest::new(
            "OLDOLD5",
            "OLDOLD5-1",
            v44::Side::Buy,
            UtcTimestamp::now(),
            v44::OrdType::Limit,
        );
        (replace.symbol, replace.order_qty, replace.price) = (Some("AAPL".into()), Some(200.into()), Some(10.into()));
        store_sent(&venues, &id(), &[order("OLDOLD5"), replace.to_message()]);
        let found = gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), venues)]).await.unwrap();
        assert_eq!(found, Orphans { scanned: 2, orders: 1, amendments: 0, unsent: 0, pruned: 0 });
        let state = gateway.lock();
        let [orphan] = state.orphan_cancels[VENUE].as_slice() else { panic!() };
        assert_eq!(orphan.cancel.get(tags::ORIG_CL_ORD_ID), Some("OLDOLD5-1"));
        assert_eq!(orphan.cancel.get(tags::ORDER_QTY), Some("200"));
        assert!(state.orphan_ids.contains("OLDOLD5"));
    }

    /// An orphan's cancel the venue's session can't take waits for its next logon.
    #[tokio::test]
    async fn an_orphan_cancel_the_session_refuses_waits_for_the_next_logon() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let gateway = restarted(&store, "NEWNEW").await;
        let venues = Arc::new(MemoryStorage::new());
        store_sent(&venues, &id(), &[order("OLDOLD1")]);
        gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), venues)]).await.unwrap();
        assert!(gateway.set_venues([(VENUE.into(), SessionHandle::disconnected(id()))]).is_empty());
        gateway.send_orphan_cancels(&VENUE.into());
        assert_eq!(gateway.lock().orphan_cancels[VENUE].len(), 1);
        gateway.committed().await;
        assert_eq!(store.unknown_downstreams(&["OLDOLD1".into()]).await.unwrap(), ["OLDOLD1"], "recorded unsent");
    }

    /// An orphan's cancel the venue's session takes and then drops unsent (its logon refused) is
    /// queued again for the next logon, and the orphan isn't recorded.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_orphan_cancel_dropped_unsent_waits_for_the_next_logon() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let gateway = restarted(&store, "NEWNEW").await;
        let venues = Arc::new(MemoryStorage::new());
        store_sent(&venues, &id(), &[order("OLDOLD1")]);
        gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), venues.clone())]).await.unwrap();
        let (holding, mut held) = unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let refuser = Arc::new(Refuser { holding, release: Mutex::new(Some(gate)) });
        let venue_addr = serve("VENUE", refuser).await;
        let venue = initiator_with(&venue_addr, "JETBRIDGE", "VENUE", Arc::new(VenueApp(gateway.clone())), venues);
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        tokio::spawn(venue.clone().run());
        timeout(Duration::from_secs(5), held.recv()).await.expect("logon not held");
        // Taken by the session, queued behind the logon.
        gateway.send_orphan_cancels(&VENUE.into());
        assert!(gateway.lock().orphan_cancels.is_empty());
        // Being sent: pending, though no longer waiting.
        assert_eq!(gateway.lock().orphan_cancels_sending, 1);
        assert!(gateway.orphans_pending());
        release.send(()).unwrap();
        let requeued = async {
            loop {
                let requeued = {
                    let state = gateway.lock();
                    state.orphan_cancels.get(VENUE).is_some_and(|cancels| !cancels.is_empty())
                        && state.orphan_cancels_sending == 0
                };
                if requeued {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(Duration::from_secs(5), requeued).await.expect("not queued again");
        venue.shutdown(None).await;
        gateway.committed().await;
        assert_eq!(store.unknown_downstreams(&["OLDOLD1".into()]).await.unwrap(), ["OLDOLD1"]);
    }

    /// A venue holding a request under this start's prefix stops the start: its IDs would be
    /// this start's orders'.
    #[tokio::test]
    async fn a_request_under_this_starts_prefix_stops_the_start() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let gateway = restarted(&store, "SAMESS").await;
        let venues = Arc::new(MemoryStorage::new());
        store_sent(&venues, &id(), &[order("SAMESS1")]);
        let error = gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), venues)]).await.unwrap_err();
        assert!(error.to_string().contains("under this start's ID prefix"), "{error}");
    }

    /// A venue's store with no record of the session (a memory store after a restart) stops the
    /// start when the database has orders for the venue: its scan would find nothing. One with a
    /// record, or with no orders to look for, doesn't.
    #[tokio::test]
    async fn a_venue_store_with_no_record_of_its_session_stops_the_start() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let fresh = || -> Arc<dyn SessionStorage> { Arc::new(MemoryStorage::new()) };
        assert!(
            restarted(&store, "FIRSTS").await.cancel_orphans(&store, vec![(VENUE.into(), id(), fresh())]).await.is_ok()
        );
        let gateway = restarted(&store, "SECOND").await;
        book_order(&gateway);
        let changes = gateway.lock().book.take_changes();
        store.write(&[changes]).await.unwrap();
        let gateway = restarted(&store, "THIRDS").await;
        let error = gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), fresh())]).await.unwrap_err();
        assert!(error.to_string().contains("no record of the session"), "{error}");
        let recorded = Arc::new(MemoryStorage::new());
        store_sent(&recorded, &id(), &[]);
        assert!(gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), recorded)]).await.is_ok());
    }

    /// Requests a venue's session stored of an order the database has since pruned aren't
    /// orphans, though the database no longer knows them, even stamped up to the slack after the
    /// book's send time; an orphan sent past it still is. A live order's requests as old are
    /// still seen: its unanswered order isn't sent again, and its orphaned replace goes to the
    /// book.
    #[tokio::test]
    async fn requests_of_pruned_orders_arent_orphans() {
        use crate::store::tests::{current, dated};
        let store = Store::open("sqlite::memory:").await.unwrap();
        let at = |minutes: i64| DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::minutes(minutes);
        let funda = crate::book::ClientId::from("FUNDA");
        let (day1, day3) = (crate::book::Day(at(60)), crate::book::Day(at(180)));
        let mut pruned = dated(1, &funda, Some(day1), true, at(5));
        pruned.sent[0].0 = "OLDOLD1".into();
        let mut amend = pruned.sent[0].clone();
        (amend.0, amend.1.sent_at) = ("OLDOLD1-1".into(), at(7));
        pruned.sent.push(amend);
        // Live, so kept, its order unanswered and sent before the pruned order's last request.
        let mut live = dated(2, &funda, Some(day1), false, at(6));
        let row = &mut live.orders[0].order;
        (row.status, row.name, row.downstream, row.amendments) =
            (crate::book::Status::PendingNew, "OLDOLD2".into(), "OLDOLD2".into(), 0);
        (live.sent[0].0, live.sent[0].1.answered) = ("OLDOLD2".into(), false);
        // The newest order, which pruning keeps.
        let mut newest = dated(3, &funda, Some(day1), true, at(8));
        newest.sent[0].0 = "OLDOLD3".into();
        store.write(&[pruned, live, newest, current(&funda, day3)]).await.unwrap();
        assert_eq!(store.prune(at(120), 10).await.unwrap().orders, 1);
        assert_eq!(store.pruned_through().await.unwrap(), Some(at(7)));

        let gateway = restarted(&store, "NEWNEW").await;
        let venues = Arc::new(MemoryStorage::new());
        let stamped = |msg: Message, at: DateTime<Utc>| msg.with(tags::SENDING_TIME, UtcTimestamp::from(at));
        let late = chrono::Duration::seconds(30);
        let through = at(7) + orphans::PRUNED_SLACK;
        let cancel = v44::OrderCancelRequest::new("OLDOLD1", "OLDOLD1-1", v44::Side::Buy, UtcTimestamp::now());
        let mut replace = v44::OrderCancelReplaceRequest::new(
            "OLDOLD2",
            "OLDOLD2-1",
            v44::Side::Buy,
            UtcTimestamp::now(),
            v44::OrdType::Limit,
        );
        (replace.symbol, replace.order_qty, replace.price) = (Some("AAPL".into()), Some(200.into()), Some(10.into()));
        store_sent(
            &venues,
            &id(),
            &[
                stamped(order("OLDOLD1"), at(5) + late),
                stamped(order("OLDOLD2"), at(6) + late),
                // At the slack's end exactly: still the pruned order's.
                stamped(cancel.to_message(), through),
                stamped(replace.to_message(), at(7)),
                stamped(order("OLDOLD3"), at(8) + late),
                stamped(order("OLDOLD4"), through + chrono::Duration::milliseconds(1)),
            ],
        );
        let found = gateway.cancel_orphans(&store, vec![(VENUE.into(), id(), venues)]).await.unwrap();
        assert_eq!(found, Orphans { scanned: 6, orders: 1, amendments: 1, unsent: 0, pruned: 2 });
        let state = gateway.lock();
        let [orphan] = state.orphan_cancels[VENUE].as_slice() else { panic!() };
        assert_eq!(orphan.cancel.get(tags::ORIG_CL_ORD_ID), Some("OLDOLD4"));
    }

    /// A prune run deletes the done orders of days ended `retention` before now; without a
    /// retention it does nothing, and while orphan work is pending it skips the run.
    #[tokio::test]
    async fn a_prune_run_needs_a_retention_and_no_orphan_work_pending() {
        use crate::store::tests::{current, dated};
        let store = Store::open("sqlite::memory:").await.unwrap();
        let at = |minutes: i64| DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::minutes(minutes);
        let funda = crate::book::ClientId::from("FUNDA");
        let (day1, day3) = (crate::book::Day(at(60)), crate::book::Day(at(180)));
        let (old, newest) = (dated(1, &funda, Some(day1), true, at(5)), dated(2, &funda, Some(day1), true, at(8)));
        store.write(&[old, newest, current(&funda, day3)]).await.unwrap();
        let now = at(120) + chrono::Duration::days(1);
        let gateway =
            Arc::try_unwrap(restarted(&store, "NEWNEW").await).unwrap().with_clock(Clock::from_fn(move || now));
        let day = Duration::from_secs(86_400);

        assert_eq!(gateway.prune_once(&store, None).await, None);
        gateway.lock().orphan_cancels_sending = 1;
        assert!(gateway.orphans_pending());
        gateway.lock().orphan_cancels_sending = 0;
        let unsent = orphans::Unsent { downstreams: vec!["1".into()], next_outgoing: 1 };
        gateway.lock().unsent.insert(VENUE.into(), unsent);
        assert_eq!(gateway.prune_once(&store, Some(day)).await, None);
        assert_eq!(store.pruned_through().await.unwrap(), None);

        gateway.lock().unsent.clear();
        assert!(!gateway.orphans_pending());
        assert_eq!(gateway.prune_once(&store, Some(day)).await, Some(1));
        assert_eq!(store.pruned_through().await.unwrap(), Some(at(5)));
    }

    /// Our session with the venue, as the gateway's initiator has it.
    fn id() -> SessionId {
        SessionId::new("FIX.4.4", "JETBRIDGE", "VENUE")
    }

    /// A cancel the venue's session stored that the database never had, of an order it has: the
    /// order gets the gateway's cancel when the venue logs on, and the venue's Canceled under the
    /// orphan's ID ends it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_orphaned_cancel_cancels_its_order_and_the_venues_answer_applies() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let before = restarted(&store, "OLDOLD").await;
        let mut sessions = connected(&before, false).await;
        send_order(&before, "C1");
        let _: v44::NewOrderSingle = next_message(&mut sessions.venue_events).await.parse().unwrap();
        VenueApp(before.clone())
            .on_message(&mut Context::new(&sessions.venue.handle()), &venue_report("OLDOLD1", 0))
            .unwrap();
        before.committed().await;
        sessions.venue.shutdown(None).await;

        let after = restarted(&store, "NEWNEW").await;
        let venues = Arc::new(MemoryStorage::new());
        let id = SessionId::new("FIX.4.4", "JETBRIDGE", "VENUE");
        let mut orphan = v44::OrderCancelRequest::new("OLDOLD1", "OLDOLD1-1", v44::Side::Buy, UtcTimestamp::now());
        orphan.symbol = Some("AAPL".into());
        store_sent(&venues, &id, &[orphan.to_message()]);
        let found = after.cancel_orphans(&store, vec![(VENUE.into(), id, venues.clone())]).await.unwrap();
        assert_eq!(found, Orphans { scanned: 1, orders: 0, amendments: 1, unsent: 0, pruned: 0 });
        assert_eq!(unsent(&after), [true]);
        after.committed().await;
        assert!(store.unknown_downstreams(&["OLDOLD1-1".into()]).await.unwrap().is_empty(), "recorded");

        let (addr, mut events) = venue_expecting(2).await;
        start_venue(&after, &addr, venues);
        let cancel: v44::OrderCancelRequest =
            next_of_type(&mut events, MsgType::OrderCancelRequest).await.parse().unwrap();
        assert_eq!((cancel.cl_ord_id.as_str(), cancel.orig_cl_ord_id.as_str()), ("OLDOLD1-2", "OLDOLD1"));
        let mut canceled = v44::ExecutionReport::new(
            "V-1",
            "X-C",
            v44::ExecType::Canceled,
            v44::OrdStatus::Canceled,
            v44::Side::Buy,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
        );
        (canceled.cl_ord_id, canceled.orig_cl_ord_id) = (Some("OLDOLD1-1".into()), Some("OLDOLD1".into()));
        let venue = after.lock().venues[VENUE].session.clone();
        VenueApp(after.clone()).on_message(&mut Context::new(&venue), &canceled.to_message()).unwrap();
        assert_eq!(after.lock().book.order(OrderId(1)).unwrap().status, crate::book::Status::Canceled);
        assert!(after.cancelling().is_empty());
    }

    /// At shutdown, a restored client's working order is cancelled though it never logged on
    /// since: its orders are as unattended as anyone's.
    #[tokio::test]
    async fn shutdown_cancels_the_working_orders_of_restored_clients() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let before = restarted(&store, "OLDOLD").await;
        book_order(&before);
        let changes = before.lock().book.take_changes();
        store.write(&[changes]).await.unwrap();
        let after = restarted(&store, "NEWNEW").await;
        after.cancel_at_shutdown(|comp_id| comp_id == "FUNDA");
        assert_eq!(unsent(&after), [true]);
    }

    type Events = broadcast::Receiver<GatewayEvent>;

    /// Every event published so far, without waiting.
    fn published(events: &mut Events) -> Vec<GatewayEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    /// The kinds of the alarms published so far, and the CompIDs of their clients.
    fn alarms(events: &mut Events) -> Vec<(AlarmKind, Option<String>)> {
        let alarms = published(events).into_iter().filter_map(|event| match event {
            GatewayEvent::Alarm(alarm) => Some((alarm.kind, alarm.client.map(|client| client.comp_id().to_string()))),
            _ => None,
        });
        alarms.collect()
    }

    fn statuses(events: &mut Events) -> Vec<(u64, crate::book::Status)> {
        let views = published(events).into_iter().filter_map(|event| match event {
            GatewayEvent::Order(view) => Some((view.id.0, view.status)),
            _ => None,
        });
        views.collect()
    }

    /// An order's steps (sent, acknowledged, part filled) each publish its view, in that order.
    #[tokio::test]
    async fn an_orders_steps_publish_its_views_in_order() {
        use crate::book::Status;
        let gateway = gateway();
        let mut events = gateway.subscribe();
        let mut sessions = connected(&gateway, false).await;
        let venue = VenueApp(gateway.clone());
        let venue_message = |msg: &Message| venue.on_message(&mut Context::new(&sessions.venue.handle()), msg).unwrap();
        send_order(&gateway, "C1");
        let _: v44::NewOrderSingle = next_message(&mut sessions.venue_events).await.parse().unwrap();
        venue_message(&venue_report("1", 0));
        venue_message(&venue_report("1", 10));
        let want = [(1, Status::PendingNew), (1, Status::New), (1, Status::PartiallyFilled)];
        assert_eq!(statuses(&mut events), want);
    }

    /// Order views are built only while someone listens: none before, some during, none after.
    #[tokio::test]
    async fn without_a_listener_no_view_is_built() {
        let gateway = gateway();
        let built = || gateway.views_built.load(Ordering::Relaxed);
        gateway.handle(new_order_from("FUNDA", "C1"));
        assert_eq!(built(), 0);
        let mut events = gateway.subscribe();
        gateway.handle(new_order_from("FUNDA", "C2"));
        assert_eq!(built(), 1);
        assert_eq!(statuses(&mut events), [(2, crate::book::Status::Rejected)]);
        drop(events);
        gateway.handle(new_order_from("FUNDA", "C3"));
        assert_eq!(built(), 1);
    }

    #[tokio::test]
    async fn a_block_and_an_unblock_are_published_once_each() {
        let gateway = gateway();
        let mut events = gateway.subscribe();
        gateway.block(Scope::All).await;
        gateway.block(Scope::All).await;
        gateway.unblock(Scope::All).await;
        gateway.unblock(Scope::All).await;
        let want = [GatewayEvent::Blocked { scope: Scope::All }, GatewayEvent::Unblocked { scope: Scope::All }];
        assert_eq!(published(&mut events), want);
    }

    #[test]
    fn events_are_json_tagged_by_type() {
        use crate::book::Alarm;
        let blocked = serde_json::to_value(GatewayEvent::Blocked { scope: Scope::All }).unwrap();
        assert_eq!(blocked, serde_json::json!({"type": "blocked", "scope": "all"}));
        let time = at("2026-10-11T12:00:00Z");
        let alarm = Alarm { time, kind: AlarmKind::StuckCancel, client: Some("FUNDA".into()), text: "t".into() };
        let alarm = serde_json::to_value(GatewayEvent::Alarm(alarm)).unwrap();
        let client = "FIX.4.4:JETBRIDGE->FUNDA";
        let want = serde_json::json!(
            {"type": "alarm", "time": "2026-10-11T12:00:00Z", "kind": "stuck_cancel", "client": client, "text": "t"}
        );
        assert_eq!(alarm, want);
        let alarm = Alarm { time, kind: AlarmKind::Orphan, client: None, text: "t".into() };
        assert_eq!(serde_json::to_value(GatewayEvent::Alarm(alarm)).unwrap()["client"], serde_json::Value::Null);
        let gateway = gateway();
        book_order(&gateway);
        let view = gateway.lock().book.touched_views().remove(0);
        let order = serde_json::to_value(GatewayEvent::Order(view)).unwrap();
        assert_eq!((&order["type"], &order["id"], &order["order_id"]), (&"order".into(), &1.into(), &"1".into()));
    }

    #[tokio::test]
    async fn a_cancel_given_up_raises_a_stuck_cancel() {
        let gateway = gateway();
        book_order(&gateway);
        // Into the book, so its send isn't refused for want of a venue.
        book_input(&gateway, Input::CancelOrder { order: OrderId(1) });
        let mut events = gateway.subscribe();
        let refused = Input::VenueCancelReject {
            venue: VENUE.into(),
            downstream: "1-1".into(),
            status: Some(crate::book::Status::New),
            text: None,
            reason: Some(1),
            venue_order_id: None,
        };
        gateway.handle(refused);
        assert_eq!(alarms(&mut events), [(AlarmKind::StuckCancel, Some("FUNDA".into()))]);
    }

    #[tokio::test]
    async fn a_venue_report_for_no_order_raises_an_unknown_order() {
        let gateway = gateway();
        let mut events = gateway.subscribe();
        let input = Version::Fix44.venue_input(&VENUE.into(), &venue_report("99", 0)).unwrap().unwrap();
        gateway.handle(input);
        assert_eq!(alarms(&mut events), [(AlarmKind::UnknownOrder, None)]);
    }

    #[tokio::test]
    async fn a_fill_past_the_orders_quantity_raises_an_overfill() {
        let gateway = gateway();
        book_order(&gateway);
        let mut events = gateway.subscribe();
        let input = Version::Fix44.venue_input(&VENUE.into(), &venue_report("1", 150)).unwrap().unwrap();
        gateway.handle(input);
        assert_eq!(alarms(&mut events), [(AlarmKind::Overfill, Some("FUNDA".into()))]);
        assert_eq!(gateway.lock().book.order(OrderId(1)).unwrap().cum_qty, Decimal::ZERO);
    }

    /// Orphans found by the book (a venue's cancel the store lacks) and by the gateway (a report
    /// on an orphaned order) each raise one.
    #[tokio::test]
    async fn an_orphaned_request_and_a_report_on_one_raise_orphans() {
        let gateway = gateway();
        book_order(&gateway);
        let mut events = gateway.subscribe();
        gateway.handle(Input::Orphan { venue: VENUE.into(), downstream: "1-1".into(), replace: None });
        assert_eq!(alarms(&mut events), [(AlarmKind::Orphan, None)]);
        gateway.lock().orphan_ids.insert("X1".into());
        let input = Version::Fix44.venue_input(&VENUE.into(), &venue_report("X1", 0)).unwrap().unwrap();
        assert!(gateway.orphan_answer(&VENUE.into(), &input));
        assert_eq!(alarms(&mut events), [(AlarmKind::Orphan, None)]);
    }

    #[tokio::test]
    async fn a_venue_session_reset_since_the_store_was_read_raises_a_venue_store_reset() {
        let gateway = gateway();
        let sessions = connected(&gateway, false).await;
        let mut events = gateway.subscribe();
        let unsent = orphans::Unsent { downstreams: vec!["1".into()], next_outgoing: u64::MAX };
        let inputs = gateway.unsent_inputs(&VENUE.into(), unsent, &sessions.venue.handle());
        assert!(matches!(inputs[..], [Input::SendFailed { .. }]));
        assert_eq!(alarms(&mut events), [(AlarmKind::VenueStoreReset, None)]);
    }

    /// Raised once as the database falls behind, by a tick's step with no input, and once as it
    /// catches up; not again for each step in between.
    #[tokio::test]
    async fn the_database_falling_behind_and_catching_up_raises_a_gateway_unavailable_each() {
        let (gateway, store) = gateway_with_store(Duration::from_millis(50)).await;
        let mut events = gateway.subscribe();
        store.hooks.gate.send_replace(false);
        gateway.handle(new_order_from("FUNDA", "C1"));
        let writer = gateway.writer.as_ref().unwrap();
        let waited = async {
            while !writer.unavailable() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(Duration::from_secs(5), waited).await.expect("never unavailable");
        gateway.handle_all([]);
        gateway.handle_all([]);
        assert_eq!(alarms(&mut events), [(AlarmKind::GatewayUnavailable, None)]);
        assert!(gateway.unavailable());
        store.hooks.gate.send_replace(true);
        timeout(Duration::from_secs(5), gateway.committed()).await.expect("not committed");
        gateway.handle_all([]);
        assert_eq!(alarms(&mut events), [(AlarmKind::GatewayAvailable, None)]);
        assert!(!gateway.unavailable());
    }

    /// The tick notices the database falling behind and catching up with no order coming in.
    /// Real time, briefly: the writer ages its batches by `std::time::Instant`, which tokio's
    /// paused clock doesn't move; a zero wait makes any uncommitted batch too old.
    #[tokio::test]
    async fn the_tick_raises_gateway_unavailable_and_available_without_traffic() {
        let (gateway, store) = gateway_with_store(Duration::ZERO).await;
        let mut events = gateway.subscribe();
        store.hooks.gate.send_replace(false);
        gateway.handle(new_order_from("FUNDA", "C1"));
        let ticks = tokio::spawn(gateway.clone().run_ticks(Duration::from_millis(10)));
        let next_alarm = async |events: &mut Events| loop {
            if let GatewayEvent::Alarm(alarm) = events.recv().await.unwrap() {
                break alarm.kind;
            }
        };
        let raised = timeout(Duration::from_secs(5), next_alarm(&mut events)).await.expect("no alarm");
        assert_eq!(raised, AlarmKind::GatewayUnavailable);
        store.hooks.gate.send_replace(true);
        let cleared = timeout(Duration::from_secs(5), next_alarm(&mut events)).await.expect("no alarm");
        assert_eq!(cleared, AlarmKind::GatewayAvailable);
        ticks.abort();
    }
}
