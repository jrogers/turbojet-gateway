//! The gateway's edges: what connects the book to client and venue sessions.
//!
//! One [`Gateway`] holds the book. [`ClientApp`] serves the acceptor the clients log on to, and
//! [`VenueApp`] the initiators that log on to venues. Both call into the book inline, under its
//! lock, and send what it answers before letting go: reports through the [`Outbox`], venue
//! requests through each venue's [`SessionHandle`]. Neither waits, so the lock is held for
//! microseconds, and every client gets its reports in the order the book made them, even when
//! its own request and a venue's fill race on different threads.

pub mod outbox;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, NaiveTime, Utc};
use tracing::{error, info, warn};
use turbojet::fields::{BusinessRejectReason, CompactString, UtcTimestamp, format_compact};
use turbojet::{
    Application, Clock, ConnectionInfo, Context, Disconnect, Dropped, Message, MessageReject, Receipt, SendError,
    SessionHandle, SessionId, SessionSchedule,
};

use crate::book::{Book, ClientId, Day, GatewayCancel, Input, OrderId, Output, Scope, VenueId};
use crate::fix::{self, Version};
use crate::risk;
use crate::router::Router;
pub use outbox::Outbox;

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
    /// Each client session seen since start, made once: see [`client`](Self::client).
    clients: Mutex<HashMap<SessionId, ClientId>>,
    /// Every client's: the acceptor's BeginString.
    client_version: Version,
    /// For TransactTime(60) on what the gateway sends.
    clock: Clock,
    /// A refused send left a gateway cancel waiting since the last retry tick (see
    /// [`Book::take_cancel_unsent`]): the tick looks at the book only then.
    cancels_waiting: AtomicBool,
}

/// What a stop would lose until the book is persisted: see [`Gateway::unfinished`].
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
        };
        Self {
            state: Mutex::new(state),
            outbox: Outbox::new(),
            clients: Mutex::default(),
            client_version,
            clock: Clock::system(),
            cancels_waiting: AtomicBool::new(false),
        }
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

    /// What a stop would lose until the book is persisted.
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
    /// [`cancel_all`](Self::cancel_all).
    pub fn block(self: &Arc<Self>, scope: Scope) {
        warn!(?scope, "blocking clients");
        self.handle_all([Input::Block { scope }]);
    }

    /// The kill switch: blocks the clients in `scope` and cancels their live orders at the
    /// venues, as one step, so no order slips in between. Each client hears of each order's end
    /// as the venue's Canceled comes back. [`cancelling`](Self::cancelling) lists the cancels
    /// still going.
    pub fn cancel_all(self: &Arc<Self>, scope: Scope) {
        warn!(?scope, "blocking clients and cancelling their orders");
        self.handle_all([Input::Block { scope: scope.clone() }, Input::CancelAll { scope }]);
    }

    /// Lifts the block of exactly `scope`: a block of a session's CompID, or of everyone, stays.
    /// `false` if `scope` wasn't blocked, which changes nothing.
    pub fn unblock(&self, scope: Scope) -> bool {
        let mut state = self.lock();
        if !state.book.blocked().contains(&scope) {
            info!(?scope, blocked = ?state.book.blocked(), "nothing to unblock");
            return false;
        }
        warn!(?scope, "unblocking clients");
        let mut out = Vec::new();
        state.book.handle(Input::Unblock { scope }, &mut out);
        debug_assert!(out.is_empty(), "an unblock sends nothing");
        true
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
        let sessions: Vec<ClientId> = {
            let clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
            clients.iter().filter(|(id, _)| on(&id.target_comp_id)).map(|(_, client)| client.clone()).collect()
        };
        let mut state = self.lock();
        let working = state.book.working_by_client();
        let orders: BTreeMap<ClientId, usize> =
            sessions.into_iter().filter_map(|client| Some((client.clone(), *working.get(&client)?))).collect();
        if orders.is_empty() {
            return;
        }
        warn!(working = ?orders, "shutting down: cancelling the working orders of clients with cancel on disconnect");
        let inputs = orders.into_keys().map(|client| Input::CancelAll { scope: Scope::Session(client) });
        self.handle_locked(&mut state, inputs);
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
    /// its CompID has fewer than [`MAX_SESSIONS_PER_COMP_ID`] and its SubID and LocationID have
    /// no '/'. With one, `FUNDA` SubID `A/B` and `FUNDA` SubID `A` LocationID `B` print the same,
    /// and would be one client. Records the session, checked and added under one lock.
    fn admit(&self, id: &SessionId) -> Result<(), String> {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if clients.contains_key(id) {
            return Ok(());
        }
        if [&id.target_sub_id, &id.target_location_id].into_iter().flatten().any(|part| part.contains('/')) {
            return Err(format!("session {id}: a SubID or LocationID with '/' isn't accepted"));
        }
        // ponytail: a scan of every session, at logon only; a count per CompID if there are many.
        let sessions = clients.keys().filter(|known| known.target_comp_id == id.target_comp_id).count();
        if sessions >= MAX_SESSIONS_PER_COMP_ID {
            return Err(format!("session {id}: its CompID has {sessions} sessions already, the most allowed"));
        }
        clients.insert(id.clone(), ClientId::new(id));
        Ok(())
    }

    /// The client on the acceptor's session `id`. Made at the session's first logon or message
    /// and kept, so a message costs a lookup and a clone rather than formatting the session. Kept
    /// for every session seen since start, as the outbox and the book keep theirs.
    fn client(&self, id: &SessionId) -> ClientId {
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
        self.handle_locked(&mut self.lock(), inputs);
    }

    /// Handles `input` from `client`, rolling the client's trading day over first if it has ended,
    /// so the input lands in the day it arrived in.
    fn handle_from(self: &Arc<Self>, client: &ClientId, input: Input) {
        let mut state = self.lock();
        let new_day = new_day(&state, client, self.clock.now()).map(|day| Input::DaysStarted { days: vec![day] });
        self.handle_locked(&mut state, new_day.into_iter().chain([input]));
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
            self.handle_locked(&mut state, [Input::DaysStarted { days }]);
        }
    }

    /// As [`handle_all`](Self::handle_all), under the lock held as `state`.
    fn handle_locked(self: &Arc<Self>, state: &mut State, inputs: impl IntoIterator<Item = Input>) {
        // After the lock, so TransactTimes rise in the order the book answers.
        let now = UtcTimestamp::from(self.clock.now());
        let mut inputs = VecDeque::from_iter(inputs);
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
        if state.book.take_cancel_unsent() {
            self.cancels_waiting.store(true, Ordering::Release);
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
        }
    }

    /// One tick's retry of the gateway's cancels: see [`run_ticks`](Self::run_ticks). Takes the book's lock only after a
    /// refused send. A refusal during this one sets the flag again for the next tick.
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

    /// Sends `output` where it's going. `Some` for a venue request its session refused.
    fn send(self: &Arc<Self>, state: &State, output: &Output, now: UtcTimestamp) -> Option<Input> {
        match output {
            Output::Report { client, .. } | Output::CancelReject { client, .. } => {
                let msg = self.client_version.client_message(output, now).expect("a client-bound output");
                // Not `ctx.send`, even to the client whose request this answers: the outbox keeps
                // each client's reports in the book's order. The cost: the client's request can
                // be committed before its report is stored, so a stop in between loses the
                // report. Stage 4, persisting the book with undelivered reports, closes that.
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
                match to.session.send(msg) {
                    Ok(receipt) => {
                        self.settle(receipt, venue.clone(), downstream.clone());
                        None
                    }
                    Err(SendError::Full(_)) => Some(failed(VENUE_BUSY)),
                    // NotConnected.
                    Err(_) => Some(failed(VENUE_UNAVAILABLE)),
                }
            }
        }
    }

    /// Waits for the venue's session to store a request, and if it drops it unsent, tells the
    /// book, so the client hears rather than waiting on an order the venue never saw.
    ///
    /// ponytail: one task per venue request; a per-venue drain like the outbox's if it measures.
    fn settle(self: &Arc<Self>, receipt: Receipt, venue: VenueId, downstream: CompactString) {
        let gateway = self.clone();
        tokio::spawn(async move {
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

/// The acceptor's application: clients, each its own session (see [`ClientId`]).
#[derive(Debug, Clone)]
pub struct ClientApp(pub Arc<Gateway>);

impl Application for ClientApp {
    fn verify_logon(&self, session: &SessionHandle, _logon: &Message, _: &ConnectionInfo) -> Result<(), String> {
        self.0.admit(session.id())
    }

    fn on_logon(&self, session: &SessionHandle) {
        self.0.outbox.logged_on(&self.0.client(session.id()), session);
    }

    /// Cancels the session's working orders at the venues, without a block: the client may log
    /// back on and trade. It hears of each order's end through the outbox, when it's back.
    fn on_cancel_on_disconnect(&self, session: &SessionHandle, ended: Disconnect) {
        warn!(%session, ?ended, "cancelling the orders of a client that didn't come back");
        let client = self.0.client(session.id());
        self.0.handle(Input::CancelAll { scope: Scope::Session(client) });
    }

    fn on_message(&self, ctx: &mut Context<'_>, msg: &Message) -> Result<(), MessageReject> {
        let client = self.0.client(ctx.session_id());
        let redelivered = ctx.maybe_redelivered() || fix::possibly_resent(msg);
        let mut input = self.0.client_version.client_input(&client, msg, redelivered)?;
        if let Input::NewOrder { redelivered: true, sent, .. } = &mut input {
            *sent = fix::first_sent(msg);
        }
        self.0.handle_from(&client, input);
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

    /// The gateway's cancels that couldn't be sent to the venue go now.
    fn on_logon(&self, session: &SessionHandle) {
        let venue = self.0.lock().venue_names.get(session.id()).map(|(venue, _)| venue.clone());
        if let Some(venue) = venue {
            self.0.handle(Input::VenueUp { venue });
        }
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
    use crate::book::Scope;
    use crate::router::{Rule, Target};

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

    /// The session would print as FUNDA's DESK SubID with LocationID 1, which is another session.
    #[test]
    fn a_sub_id_or_location_id_with_a_slash_is_refused() {
        let app = ClientApp(gateway());
        assert!(logon(&app, &desk("DESK/1")).is_err());
        let located = SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA").with_target_location_id("LDN/2");
        assert!(logon(&app, &SessionHandle::disconnected(located)).is_err());
        assert_eq!(logon(&app, &desk("DESK")), Ok(()));
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

    /// A client or venue in the test: reports what it sees, and as a venue, acks and fills every
    /// order in full at 10.
    struct Peer {
        events: UnboundedSender<Event>,
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
        assert!(gateway.set_venues([(VENUE.into(), venue.handle())]).is_empty());
        let (client, mut events) = client(&gateway).await;

        client.handle().send(order("C1")).unwrap();
        let rejected = report(&mut events).await;
        assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
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
        assert_eq!((rejected.cl_ord_id.as_deref(), rejected.exec_type), (Some("C1"), v44::ExecType::Rejected));
        assert!(rejected.text.as_deref().unwrap().starts_with(VENUE_UNAVAILABLE), "{:?}", rejected.text);
        let refused: v44::OrderCancelReject = answers[1].parse().unwrap();
        assert_eq!((refused.cl_ord_id.as_str(), refused.orig_cl_ord_id.as_str()), ("C2", "C1"));
    }

    /// Order 1 in the book, from FUNDA, as if sent to [`VENUE`]: what the book asked to send is
    /// dropped.
    fn book_order(gateway: &Gateway) {
        let request = crate::book::OrderRequest {
            cl_ord_id: "C1".into(),
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
        gateway.cancel_all(Scope::All);
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

    #[test]
    fn unblock_says_whether_the_scope_was_blocked() {
        let gateway = gateway();
        gateway.block(Scope::All);
        assert!(!gateway.unblock(Scope::CompId("FUNDA".into())));
        assert!(gateway.unblock(Scope::All));
        assert!(gateway.blocked().is_empty());
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
}
