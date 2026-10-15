//! Orphans: requests a venue's session stored, and so may have sent, that the database never
//! recorded, because the gateway stopped within a commit of sending them. Everything else the
//! database lacks is redelivered by the sessions, but the venue may be working an orphan with no
//! order in the book to report it to. At a start, before the venues' initiators open their
//! sessions, [`Gateway::cancel_orphans`] reads the requests each venue's session stored (from its
//! checkpoint: see below), groups them by order (the root of their ClOrdIDs), and cancels the
//! orders of those the database has never had, logging each at error:
//!
//! - an order the database never had (its NewOrderSingle orphaned, and perhaps replaces of it
//!   too) has no order in the book: an OrderCancelRequest naming the newest order or replace of
//!   it goes to the venue at its next logon, under a ClOrdID of this start's (`<prefix>C<n>`, a
//!   shape the scan ignores). The venue's answers, and any reports on the order, are logged at
//!   error, not passed to the book;
//! - an orphaned cancel or replace of an order the book holds is [`Input::Orphan`]: the venue's
//!   answers to it apply to the order. A cancel's order the gateway cancels at the venue's
//!   logon; a replace's keeps working.
//!
//! Each orphan is recorded once handled (see [`Changes::orphans`]), so a restart doesn't take it
//! again.
//!
//! A venue's store may still hold requests of orders [`Store::prune`] has deleted, whose
//! ClOrdIDs the database no longer knows. An unknown order whose requests were all sent no later
//! than [`Store::pruned_through`], with a minute's slack, isn't an orphan: its requests are
//! counted as pruned, and left alone.
//!
//! The scan also finds the converse: requests the database has as sent, unanswered, that the
//! venue's session never stored, so never sent (the gateway stopped after their change committed,
//! before the session stored them). They go at the venue's next logon as they were
//! ([`Input::Unsent`]). Only those sent since the store was created or last reset: one missing
//! from before then was lost to a reset (a schedule's, or an operator's), and the venue had it, so
//! it's left pending, logged; a store that doesn't record when it was created sends none. One
//! evicted from a store past its size budget since its last reset looks unsent, and goes again:
//! the venue may refuse it as a duplicate.
//!
//! The scan reads each venue's store from its checkpoint (see [`Gateway::checkpoint_once`]): every
//! request of ours stored below it has its change committed, and is no longer unanswered
//! (answered, its order done, or an original order past PendingNew), so it can be neither an
//! orphan nor unsent. A checkpoint is ignored, and the whole store read, if the store was created
//! or reset after it was taken, or within a millisecond before, doesn't say when it was created,
//! or is numbered short of it. Unanswered requests the scan finds are bounded again (`State::in_flight`), so a later
//! checkpoint doesn't pass them. A request's bound is its session's next number as last read, at
//! its logon or the last checkpoint's run, not at each send, which would cost Turbojet's registry
//! locks per request: so under steady load a checkpoint lags a run behind, and a crash leaves
//! about two intervals' messages to scan, not one.
//!
//! ponytail: a whole scan, in windows of [`SCAN_WINDOW`], costs about 3.9 s per million stored
//! NewOrderSingles, all ours (2.2 s reading and decoding, 1.7 s looking up), a release build on an
//! M3 from a `MemoryStorage` and an in-memory SQLite, the machine not idle. Accepted ceilings: a
//! request its venue never answers holds the checkpoint back, so the scan grows back towards a
//! whole one; orphan work pending for any venue (one that never logs on again, say) holds back
//! every venue's checkpoint, not just its own, so until it clears each start reads more of every
//! store, back towards the whole, as before checkpoints; `Gateway::check_prefix`, the backstop for
//! a clock that went back, sees only the scanned tail; a counterparty's ResetSeqNumFlag mid-session
//! resets the store with no logout, leaving bounds read before it too high: the next checkpoint's
//! run catches it (the session's number fell), bounds the venue's requests at 1 and takes no
//! checkpoint of it, unless the session sent more after the reset than before it within that
//! interval, when a checkpoint can pass a request (exact would be a reset count, or the store's
//! created time, in Turbojet's `Activity`: an engine change); a clock stepped back across a reset,
//! with no checkpoint taken after the reset before a stop, can make the stale checkpoint look newer
//! than the store; and checkpoints are keyed by venue name, so a venue pointed at another session's
//! store, older and numbered further, passes both checks (the rest of the orphan logic, too, takes
//! a venue and its session as one).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use std::io;
use std::sync::Arc;

use tracing::{debug, error, info, warn};
use turbojet::codec::{self, Decoded};
use turbojet::fields::{CompactString, UtcTimestamp, format_compact};
use turbojet::message::tags;
use turbojet::store::{Fetched, Opened, SentMessages, SessionLog};
use turbojet::{Message, MsgType, Receipt, SessionHandle, SessionId, SessionStorage};

use super::{Gateway, State};
use crate::book::{AlarmKind, Changes, Checkpoint, ClientId, Input, OrdType, OrphanedReplace, VenueId};
use crate::fix::Version;
use crate::store::Store;

/// Sequence numbers read from a venue's store at once: a window's messages are decoded and
/// dropped before the next is read, so the scan keeps only the number, kind and ClOrdID of each
/// request of ours, whatever the store holds.
pub const SCAN_WINDOW: u64 = 10_000;

/// What [`Gateway::cancel_orphans`] found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Orphans {
    /// Messages read from the venues' stores.
    pub scanned: usize,
    /// Orders the database never had, whose cancels wait for the venue's logon.
    pub orders: usize,
    /// Cancels and replaces of orders the book holds, taken by the book.
    pub amendments: usize,
    /// Requests the database has that the venues' sessions never stored, to send at their logons.
    pub unsent: usize,
    /// Requests of orders pruned from the database, left alone.
    pub pruned: usize,
}

/// How long after [`Store::pruned_through`] a stored request's SendingTime(52) may be and still
/// be taken for a pruned order's: that time is the book's, and Turbojet stamps SendingTime a
/// little later, as it encodes. An orphan is sent just before a stop, and found at the next
/// start; a pruned order's requests are over a day older than the prune, retention being at least
/// a day. That holds as long as pruning never runs while orphans are unrecorded: an orphaned
/// order found at a start is recorded only once its cancel is stored, at the venue's logon. So the
/// first prune comes an hour after the start, after `cancel_orphans`, and a run is skipped while
/// the gateway has orphan work pending. The other way wrong is harmless: a SendingTime stamped
/// more than the slack late, behind a throttle's backlog, only means a needless cancel of a done
/// order, which the venue refuses.
pub(super) const PRUNED_SLACK: Duration = Duration::from_secs(60);

/// Requests the database has as sent that a venue's session never stored, waiting for its logon:
/// see [`Gateway::unsent_inputs`].
#[derive(Debug)]
pub(super) struct Unsent {
    pub(super) downstreams: Vec<CompactString>,
    /// The session's next outgoing number when its store was read: no higher after the logon,
    /// whose Logon takes one, the store was reset in between.
    pub(super) next_outgoing: u64,
}

/// Text(58) for a request not sent again because the venue's session was reset since.
const NOT_SENT_RESET: &str = "not sent before the gateway restarted, and the venue's session has been reset since";

/// The cancel of an order the database never had, waiting for its venue's logon.
#[derive(Debug)]
pub(super) struct OrphanCancel {
    pub(super) cancel: Message,
    /// The order's requests the venue's session stored, recorded once the cancel is sent.
    pub(super) chain: Vec<CompactString>,
}

/// The fields of an order or replace its cancel repeats, as FIX asks: the request's own values,
/// in the venue's version.
const REPEATED: [u32; 4] = [tags::ACCOUNT, tags::SYMBOL, tags::SIDE, tags::ORDER_QTY];

/// A request of ours a venue's session stored.
#[derive(Debug)]
struct Request {
    seq: u64,
    kind: Kind,
    cl_ord_id: CompactString,
    /// Its SendingTime(52), if it reads.
    sent: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Order,
    Cancel,
    Replace,
}

impl Gateway {
    /// Reads the messages each of `venues` (name, session and the store its initiator keeps it
    /// in) stored from its checkpoint in `store`, or from the first if it has none or it doesn't
    /// hold, and cancels the orders of the requests `store` has never recorded: see the
    /// [module documentation](self). Call it after [`restore`](Self::restore) and before the
    /// initiators start: it opens each session's log and closes it, which a `disk` store locks
    /// while open. Nothing is sent: the cancels go at each venue's logon.
    ///
    /// # Errors
    ///
    /// A venue's store or the database can't be read, or a venue holds a request under this
    /// start's prefix: a backstop, since each start takes a second after the last's (see
    /// `service`), unless the clock went back.
    pub async fn cancel_orphans(
        self: &Arc<Self>,
        store: &Store,
        venues: Vec<(String, SessionId, Arc<dyn SessionStorage>)>,
    ) -> io::Result<Orphans> {
        let mut found = Orphans::default();
        let pruned_through = store.pruned_through().await?.map(|through| through + PRUNED_SLACK);
        let checkpoints = store.checkpoints().await?;
        for (venue, id, storage) in venues {
            let venue: VenueId = venue.as_str().into();
            let in_store = |e: io::Error| io::Error::other(format!("venue {venue}'s store: {e}"));
            let mut log = open(storage.as_ref(), &id).await.map_err(in_store)?;
            // A store that has never had the session (it records when it was created at its first
            // logon) can't hold the orders the book has for the venue: a memory store, or one whose
            // files are gone. Its scan would find nothing, wrongly.
            if log.created_at().is_none() && self.lock().book.holds_orders_for(&venue) {
                return Err(in_store(io::Error::other(
                    "it has no record of the session, though the database has orders for the venue: with a \
                     [database], a venue's session must be kept in a durable store (kind = \"disk\", say), and \
                     its files kept",
                )));
            }
            let created = log.created_at().map(DateTime::<Utc>::from);
            let begin = match scan_from(checkpoints.get(&venue), created, log.next_outgoing()) {
                Ok(begin) => {
                    let next_outgoing = log.next_outgoing();
                    info!(%venue, begin, next_outgoing, "scanning the venue's store from its checkpoint");
                    begin
                }
                Err(why) => {
                    info!(%venue, "scanning the venue's whole store: {why}");
                    1
                }
            };
            let requests = scan(log.as_mut(), &venue, begin, &mut found.scanned).await.map_err(in_store)?;
            self.check_prefix(&venue, &requests)?;
            let (mut ids, seqs): (Vec<CompactString>, Vec<u64>) =
                requests.iter().map(|request| (request.cl_ord_id.clone(), request.seq)).unzip();
            found.unsent += self.find_unsent(&venue, &ids, created, log.next_outgoing());
            let chains = chains(requests);
            ids.extend(chains.keys().cloned());
            let unknown: HashSet<CompactString> = store.unknown_downstreams(&ids).await?.into_iter().collect();
            for (root, chain) in chains {
                // Only a whole unknown order: an unknown amendment of a known one is never a
                // pruned order's, however old (an orphaned replace waiting on an earlier one).
                if unknown.contains(&root) && pruned_through.is_some_and(|through| pruned(&chain, through)) {
                    found.pruned += chain.len();
                    continue;
                }
                if unknown.contains(&root) {
                    let taken = self.orphaned_order(log.as_mut(), &venue, chain, found.orders).await?;
                    found.orders += usize::from(taken);
                    continue;
                }
                for request in chain.into_iter().filter(|request| unknown.contains(&request.cl_ord_id)) {
                    found.amendments += usize::from(self.orphaned_amendment(log.as_mut(), &venue, request).await?);
                }
            }
            // The zip stops before the chains' roots, appended to `ids`.
            self.seed_in_flight(&venue, ids.iter().zip(seqs));
        }
        info!(
            scanned = found.scanned,
            orders = found.orders,
            amendments = found.amendments,
            unsent = found.unsent,
            pruned = found.pruned,
            "looked for venue requests the database lacks, and the converse"
        );
        self.lock().orphans_found = found;
        Ok(found)
    }

    /// Bounds each of `stored` (ClOrdIDs, with the numbers `venue`'s session stored them under)
    /// that holds a checkpoint back ([`Book::holds_checkpoint`](crate::book::Book::holds_checkpoint)),
    /// so none passes it: those restored from the database, and orphaned amendments now pending,
    /// or waiting to be. Requests sent again at the logon get theirs as sent.
    fn seed_in_flight<'a>(&self, venue: &VenueId, stored: impl Iterator<Item = (&'a CompactString, u64)>) {
        if self.writer.is_none() {
            return;
        }
        let mut state = self.lock();
        let state = &mut *state;
        let book = &state.book;
        let bounds = state.in_flight.entry(venue.clone()).or_default();
        for (downstream, seq) in stored.filter(|(downstream, _)| book.holds_checkpoint(downstream)) {
            bounds.entry(downstream.clone()).and_modify(|bound| *bound = (*bound).min(seq)).or_insert(seq);
        }
    }

    /// Keeps for `venue`'s next logon the requests the book has as sent to it, unanswered, that
    /// aren't among those its session `stored`, returning how many. Only those sent since the
    /// store was `created` (or last reset): one missing from before then was lost to a reset, and
    /// the venue had it, so it's left pending, as is everything if the store doesn't say. "Since"
    /// is a millisecond after: a store may keep its creation time in milliseconds, and a request
    /// wrongly left pending is better than one sent twice.
    fn find_unsent(
        &self,
        venue: &VenueId,
        stored: &[CompactString],
        created: Option<DateTime<Utc>>,
        next_outgoing: u64,
    ) -> usize {
        let stored: HashSet<&CompactString> = stored.iter().collect();
        let mut state = self.lock();
        let missing = state.book.unanswered(venue).into_iter().filter(|(downstream, _)| !stored.contains(downstream));
        let (unsent, reset): (Vec<_>, Vec<_>) =
            missing.partition(|(_, sent_at)| created.is_some_and(|c| *sent_at >= c + Duration::from_millis(1)));
        if !reset.is_empty() {
            let reset: Vec<_> = reset.into_iter().map(|(downstream, _)| downstream).collect();
            if created.is_none() {
                warn!(%venue, ?reset, "venue's store doesn't say when it was created: unanswered requests it lacks are left pending, not sent again");
            } else {
                warn!(%venue, ?reset, ?created, "unanswered requests sent before the venue's store was last reset: the venue had them; left pending");
            }
        }
        let unsent: Vec<CompactString> = unsent.into_iter().map(|(downstream, _)| downstream).collect();
        if unsent.is_empty() {
            return 0;
        }
        warn!(%venue, ?unsent, "the database has requests the venue's session never stored: sending them at its logon");
        let count = unsent.len();
        state.unsent.insert(venue.clone(), Unsent { downstreams: unsent, next_outgoing });
        count
    }

    /// Refuses a request under this start's prefix: its IDs would be this start's orders'.
    fn check_prefix(&self, venue: &VenueId, requests: &[Request]) -> io::Result<()> {
        let prefix = self.id_prefix();
        let mut ours = requests.iter().filter(|request| request.cl_ord_id.starts_with(prefix.as_str()));
        let Some(request) = ours.next().filter(|_| !prefix.is_empty()) else { return Ok(()) };
        Err(io::Error::other(format!(
            "venue {venue} holds request {} under this start's ID prefix {prefix}: the clock went back \
             since the last start; start again once it has passed the last start's",
            request.cl_ord_id
        )))
    }

    /// Queues the cancel of an order the database never had, naming the newest order or replace
    /// in `chain`, the `before + 1`th this start: `false` if there's none (only cancels, which
    /// are recorded as they are), or it doesn't read back.
    async fn orphaned_order(
        self: &Arc<Self>,
        log: &mut dyn SessionLog,
        venue: &VenueId,
        chain: Vec<Request>,
        before: usize,
    ) -> io::Result<bool> {
        let ids: Vec<CompactString> = chain.iter().map(|request| request.cl_ord_id.clone()).collect();
        let Some(newest) = chain.iter().rev().find(|request| request.kind != Kind::Cancel) else {
            error!(%venue, chain = ?ids, "venue holds cancels of an order the database never had; nothing to cancel");
            self.alarm(AlarmKind::Orphan, || {
                format!("venue {venue} holds cancels {ids:?} of an order the database never had")
            });
            self.record_orphans(ids);
            return Ok(false);
        };
        let Some(msg) = stored(log, newest.seq).await? else {
            self.alarm(AlarmKind::Orphan, || {
                format!(
                    "venue {venue} holds order {ids:?}, which the database never had and which doesn't read back \
                     from the venue's store, so it isn't cancelled"
                )
            });
            return Ok(false);
        };
        let cancel = cancel(&msg, format_compact!("{}C{}", self.id_prefix(), before + 1));
        error!(
            %venue,
            chain = ?ids,
            account = msg.get(tags::ACCOUNT),
            symbol = msg.get(tags::SYMBOL),
            side = msg.get(tags::SIDE),
            qty = msg.get(tags::ORDER_QTY),
            price = msg.get(tags::PRICE),
            cancel = cancel.get(tags::CL_ORD_ID),
            "venue holds an order the database never had: cancelling it at the venue's logon"
        );
        self.alarm(AlarmKind::Orphan, || {
            format!("venue {venue} holds order {ids:?}, which the database never had: cancelling it at its logon")
        });
        let mut state = self.lock();
        state.orphan_ids.extend(ids.iter().cloned());
        state.orphan_ids.insert(cancel.get(tags::CL_ORD_ID).unwrap_or_default().into());
        state.orphan_cancels.entry(venue.clone()).or_default().push(OrphanCancel { cancel, chain: ids });
        Ok(true)
    }

    /// Hands the book an orphaned cancel or replace of an order the database has: `true` if the
    /// book took it. One it doesn't is recorded, logged.
    async fn orphaned_amendment(
        self: &Arc<Self>,
        log: &mut dyn SessionLog,
        venue: &VenueId,
        request: Request,
    ) -> io::Result<bool> {
        let replace = match request.kind {
            Kind::Replace => stored(log, request.seq).await?.and_then(|msg| replace_terms(venue, &msg)),
            Kind::Cancel | Kind::Order => None,
        };
        let downstream = request.cl_ord_id;
        let mut state = self.lock();
        let input = Input::Orphan { venue: venue.clone(), downstream: downstream.clone(), replace };
        self.handle_locked(&mut state, [input], None);
        // Waiting for the replace pending before it, it's found again after a restart until then.
        let taken = state.book.sent(&downstream).is_some() || state.book.orphan_waiting(&downstream);
        if !taken {
            self.record_orphans(vec![downstream]);
        }
        Ok(taken)
    }

    /// Records `downstreams` as orphans handled, if there's a database.
    fn record_orphans(&self, downstreams: Vec<CompactString>) {
        if let Some(writer) = &self.writer {
            writer.queue(Changes { orphans: downstreams, ..Changes::default() }, None);
        }
    }

    /// At `venue`'s logon on `session`, what its session never stored becomes: the requests to
    /// send again, ahead of the gateway's waiting cancels and the orphans' cancels, as they'd have
    /// gone. Unless the session reset its store at this logon (a new schedule period, or
    /// `reset_on_logon`): then the requests are from an earlier period, and a day order going
    /// live in the next would be wrong, so each is a failed send instead. Read in `on_logon`,
    /// before anything else is sent: the session publishes its next outgoing number once its
    /// Logon is recorded, and a reset store's Logon took number 1.
    pub(super) fn unsent_inputs(&self, venue: &VenueId, unsent: Unsent, session: &SessionHandle) -> Vec<Input> {
        let next_outgoing = session.status().and_then(|status| status.activity).map(|activity| activity.next_outgoing);
        debug_assert!(next_outgoing.is_some(), "a session logged on publishes its numbers");
        let Some(next_outgoing) = next_outgoing else {
            warn!(%venue, "venue's session doesn't say its sequence numbers; requests to send again wait for its next logon");
            self.lock().unsent.insert(venue.clone(), unsent);
            return Vec::new();
        };
        // The Logon took the next number, unless the store started again from 1.
        let reset = next_outgoing <= unsent.next_outgoing;
        if reset {
            warn!(%venue, downstreams = ?unsent.downstreams, "venue's session reset at its logon: requests it never stored fail, not sent again");
            self.alarm(AlarmKind::VenueStoreReset, || {
                format!(
                    "venue {venue}'s session reset at its logon: requests {:?} it never stored fail",
                    unsent.downstreams
                )
            });
        }
        let to = |downstream: CompactString| {
            let venue = venue.clone();
            if reset {
                Input::SendFailed { venue, downstream, text: NOT_SENT_RESET.into() }
            } else {
                Input::Unsent { venue, downstream }
            }
        };
        unsent.downstreams.into_iter().map(to).collect()
    }

    /// Sends the cancels of `venue`'s orphaned orders, its session having logged on, recording
    /// each order's requests once its session has stored the cancel: until then a stop leaves the
    /// orphan to be found again. One the session refuses, or drops unsent, waits for the next
    /// logon.
    pub(super) fn send_orphan_cancels(self: &Arc<Self>, venue: &VenueId) {
        let mut state = self.lock();
        let Some(cancels) = state.orphan_cancels.remove(venue) else { return };
        let Some(to) = state.venues.get(venue) else {
            state.orphan_cancels.insert(venue.clone(), cancels);
            return;
        };
        let now = UtcTimestamp::from(self.clock.now());
        let (mut unsent, mut sending) = (Vec::new(), 0);
        for orphan in cancels {
            let cl_ord_id = orphan.cancel.get(tags::CL_ORD_ID);
            match to.session.send(orphan.cancel.clone().with(tags::TRANSACT_TIME, now)) {
                Ok(receipt) => {
                    info!(%venue, cl_ord_id, chain = ?orphan.chain, "sent the cancel of an orphaned order");
                    sending += 1;
                    self.settle_orphan_cancel(receipt, venue.clone(), orphan);
                }
                Err(e) => {
                    error!(%venue, cl_ord_id, "cancel of an orphaned order not sent; again at the next logon: {e}");
                    unsent.push(orphan);
                }
            }
        }
        state.orphan_cancels_sending += sending;
        if !unsent.is_empty() {
            state.orphan_cancels.insert(venue.clone(), unsent);
        }
    }

    /// Records `orphan`'s order once the venue's session has stored its cancel, or queues the
    /// cancel again for the next logon if the session drops it (or may not have stored it: a
    /// second cancel under the same ClOrdID is at worst refused as a duplicate). A drop racing
    /// the next logon waits for the one after.
    ///
    /// ponytail: one task per orphan's cancel, a handful at a start.
    fn settle_orphan_cancel(self: &Arc<Self>, receipt: Receipt, venue: VenueId, orphan: OrphanCancel) {
        let gateway = self.clone();
        tokio::spawn(async move {
            match receipt.await {
                Ok(_) => gateway.record_orphans(orphan.chain),
                Err(dropped) => {
                    let cl_ord_id = orphan.cancel.get(tags::CL_ORD_ID);
                    error!(%venue, cl_ord_id, "cancel of an orphaned order dropped unsent; again at the next logon: {dropped}");
                    gateway.lock().orphan_cancels.entry(venue).or_default().push(orphan);
                }
            }
            let mut state = gateway.lock();
            debug_assert!(state.orphan_cancels_sending > 0, "a cancel settled that wasn't counted as sending");
            state.orphan_cancels_sending -= 1;
        });
    }

    /// Whether `input` from `venue` is about an orphaned order or its cancel: logged at error
    /// rather than handed to the book, which doesn't know them.
    pub(super) fn orphan_answer(&self, venue: &VenueId, input: &Input) -> bool {
        let downstream = match input {
            Input::VenueReport { report, .. } => &report.downstream,
            Input::VenueCancelReject { downstream, .. } => downstream,
            _ => return false,
        };
        if !self.lock().orphan_ids.contains(downstream) {
            return false;
        }
        error!(%venue, %downstream, ?input, "venue's report on an orphaned order");
        self.alarm(AlarmKind::Orphan, || format!("venue {venue} reported on orphaned request {downstream}"));
        true
    }

    /// Whether orphan work a start found is pending: orphaned orders whose cancels the venues'
    /// sessions haven't stored yet, orphaned replaces waiting, or requests to send at a venue's
    /// logon. Pruning checks it, as `PRUNED_SLACK` needs: until handled, an orphan isn't
    /// recorded, and were a prune to raise [`Store::pruned_through`] past it and the gateway then
    /// stop, the next start would take it for a pruned order's.
    pub fn orphans_pending(&self) -> bool {
        self.orphan_work().is_some()
    }

    /// What [`orphans_pending`](Self::orphans_pending) finds, said, if anything: the venues with
    /// cancels or unsent requests waiting, the cancels being sent, and whether replaces wait.
    fn orphan_work(&self) -> Option<String> {
        orphan_work(&self.lock())
    }

    /// Deletes from `store` the done orders of trading days that ended `retention` before now, by
    /// the gateway's clock (see [`Store::prune`]). Does nothing without a `retention`, and skips
    /// the run, with a warning, while [orphan work is pending](Self::orphans_pending): a venue
    /// that never logs on again keeps pruning off. Waits first for what's recorded to commit, the
    /// orphans handled with it. Logs what it did, an error included, which waits for the next
    /// run. Returns the orders deleted, if it ran and didn't fail.
    pub async fn prune_once(&self, store: &Store, retention: Option<Duration>) -> Option<u64> {
        let retention = retention?;
        let now = self.clock.now();
        if let Some(work) = self.orphan_work() {
            warn!("not pruning the database while orphaned venue requests are handled: {work}");
            return None;
        }
        self.committed().await;
        let before = now - chrono::Duration::from_std(retention).expect("the config keeps retention to 100 years");
        match store.prune(before, PRUNE_BATCH).await {
            Ok(pruned) => {
                if pruned.orders > 0 {
                    info!(orders = pruned.orders, %before, "pruned the done orders of trading days ended before");
                }
                Some(pruned.orders)
            }
            Err(e) => {
                warn!(%before, "pruning the database failed; again at the next run: {e}");
                None
            }
        }
    }

    /// Takes a checkpoint of each logged-on venue's session store, and queues it for the
    /// database: a start's scan for orphans may begin there (see the [module
    /// documentation](self)). Every request of ours its session stores below it was handed to
    /// the session before now, so its change was queued before the checkpoint's and commits first;
    /// and none of them is unanswered, since the checkpoint is at most each unanswered request's
    /// bound (see `State::in_flight`), and what ends a request's being unanswered is final: its
    /// answer, its order done, an original order past PendingNew. Takes none without a
    /// database, or while orphan work is pending: until handled, an orphan isn't recorded, and a
    /// checkpoint past it would lose it, nor for a venue whose logon isn't done or whose store was
    /// reset mid-session since the last run. Drops answered requests' bounds whether it takes one
    /// or not. Returns what it queued.
    pub fn checkpoint_once(&self) -> Vec<(VenueId, Checkpoint)> {
        let Some(writer) = &self.writer else { return Vec::new() };
        let mut state = self.lock();
        let state = &mut *state;
        // Every run, before the checks below: a venue that never logs on again keeps orphan work
        // pending, and every other venue's bounds would pile up meanwhile. Also drops the bounds
        // of a venue a reload took out, once answered.
        // ponytail: a walk of every bound under the lock, but a venue's answer drops its own as
        // it's handled (`handle_locked`), so this finds only the few that misses (a request whose
        // order ended through another's answer, say); a sweep per order ending if it shows.
        state.in_flight.retain(|_, bounds| {
            bounds.retain(|downstream, _| state.book.holds_checkpoint(downstream));
            !bounds.is_empty()
        });
        if let Some(work) = orphan_work(state) {
            debug!("no checkpoint of the venues' stores while orphaned venue requests are handled: {work}");
            return Vec::new();
        }
        // Before any number is read: a store reset after it numbers from 1 again.
        let taken_at = self.clock.now();
        let mut checkpoints = Vec::new();
        for (venue, to) in &state.venues {
            // Until its logon is done, a session may publish the numbers of a store it's resetting.
            let Some(last_read) = state.logged_on.get_mut(to.session.id()) else { continue };
            let Some(activity) = to.session.status().and_then(|status| status.activity) else { continue };
            // What later requests to it are bounded by.
            let before = std::mem::replace(last_read, activity.next_outgoing);
            let next_outgoing = activity.next_outgoing;
            // Numbers only rise, but for a counterparty's ResetSeqNumFlag mid-session, which
            // resets the store with no logout: requests sent since were bounded by the old
            // store's number, too high, so they're bounded at 1; and no checkpoint this run, as
            // one stamped after the reset, which a start accepts, could pass them.
            if next_outgoing < before {
                warn!(
                    %venue,
                    before,
                    next_outgoing,
                    "the venue's session store was reset mid-session: its requests are bounded at 1, and no \
                     checkpoint of it is taken this run"
                );
                if let Some(bounds) = state.in_flight.get_mut(venue) {
                    bounds.values_mut().for_each(|bound| *bound = 1);
                }
                continue;
            }
            // ponytail: a request its venue never answers holds the checkpoint back for good, so
            // the scan grows back towards a whole one, as before checkpoints; a bound on the
            // wait if a venue shows it.
            let bounds = state.in_flight.get(venue).into_iter().flat_map(|bounds| bounds.values());
            let next_seq = bounds.copied().fold(activity.next_outgoing, u64::min);
            checkpoints.push((venue.clone(), Checkpoint { next_seq, taken_at }));
        }
        if !checkpoints.is_empty() {
            writer.queue(Changes { checkpoints: checkpoints.clone(), ..Changes::default() }, None);
        }
        checkpoints
    }
}

/// What [`Gateway::orphan_work`] finds, under the lock held as `state`.
fn orphan_work(state: &State) -> Option<String> {
    let replaces = state.book.orphaned_replaces_waiting();
    if state.orphan_cancels.is_empty() && state.orphan_cancels_sending == 0 && state.unsent.is_empty() && !replaces {
        return None;
    }
    let mut cancels: Vec<&VenueId> = state.orphan_cancels.keys().collect();
    let mut unsent: Vec<&VenueId> = state.unsent.keys().collect();
    cancels.sort();
    unsent.sort();
    Some(format!(
        "cancels waiting for venues {cancels:?}, {} being sent, unsent requests waiting for venues {unsent:?}, \
         replaces waiting: {replaces}",
        state.orphan_cancels_sending
    ))
}

/// Orders [`Gateway::prune_once`] deletes to a transaction: the store's one connection is the
/// book's writer's too, which waits while a batch runs, so each is kept short.
pub const PRUNE_BATCH: usize = 200;

/// Opens session `id`'s log in `storage`.
async fn open(storage: &dyn SessionStorage, id: &SessionId) -> io::Result<Box<dyn SessionLog>> {
    match storage.begin_open(id)? {
        Opened::Ready(log) => Ok(log),
        Opened::Pending(job) => job.run_async().await,
    }
}

async fn fetch(log: &mut dyn SessionLog, begin: u64, end: u64) -> io::Result<SentMessages> {
    match log.fetch(begin, end)? {
        Fetched::Ready(stored) => Ok(stored),
        Fetched::Pending(job) => job.run_async().await,
    }
}

/// Where a venue's scan begins: at its `checkpoint` if the store wasn't reset since it was taken,
/// so was created at least a millisecond before it (a store may keep the time in milliseconds,
/// as [`Gateway::find_unsent`] allows) and numbers up to it; else at 1, the error saying why.
fn scan_from(
    checkpoint: Option<&Checkpoint>,
    created: Option<DateTime<Utc>>,
    next_outgoing: u64,
) -> Result<u64, &'static str> {
    let checkpoint = checkpoint.ok_or("no checkpoint")?;
    let created = created.ok_or("the store doesn't say when it was created, so a reset can't be ruled out")?;
    if created + Duration::from_millis(1) > checkpoint.taken_at {
        return Err("the store was created or reset after the checkpoint was taken, or within a millisecond before");
    }
    if checkpoint.next_seq > next_outgoing {
        return Err("the checkpoint is past the store's next number: the store was reset since");
    }
    Ok(checkpoint.next_seq.max(1))
}

/// The requests of ours among the messages the log stored from `begin`, read [`SCAN_WINDOW`] at
/// a time, counting them into `scanned`.
async fn scan(log: &mut dyn SessionLog, venue: &VenueId, begin: u64, scanned: &mut usize) -> io::Result<Vec<Request>> {
    let last = log.next_outgoing().saturating_sub(1);
    let mut requests = Vec::new();
    let mut begin = begin;
    while begin <= last {
        let end = last.min(begin + SCAN_WINDOW - 1);
        for (seq, bytes) in fetch(log, begin, end).await? {
            *scanned += 1;
            match codec::decode(&bytes) {
                Decoded::Message(msg, _) => requests.extend(request(seq, &msg)),
                other => warn!(%venue, seq, ?other, "a stored message doesn't decode; skipped"),
            }
        }
        begin = end + 1;
    }
    Ok(requests)
}

/// The stored message `seq`, decoded; `None`, logged at error, if it's gone or doesn't decode.
async fn stored(log: &mut dyn SessionLog, seq: u64) -> io::Result<Option<Message>> {
    let stored = fetch(log, seq, seq).await?;
    match stored.first().map(|(_, bytes)| codec::decode(bytes)) {
        Some(Decoded::Message(msg, _)) => Ok(Some(msg)),
        other => {
            error!(seq, ?other, "an orphaned request doesn't read back from its venue's store");
            Ok(None)
        }
    }
}

/// `msg`, stored as `seq`, if it's a NewOrderSingle, OrderCancelRequest or
/// OrderCancelReplaceRequest under a ClOrdID of a shape this gateway issues.
fn request(seq: u64, msg: &Message) -> Option<Request> {
    let kind = match msg.msg_type() {
        MsgType::NewOrderSingle => Kind::Order,
        MsgType::OrderCancelRequest => Kind::Cancel,
        MsgType::OrderCancelReplaceRequest => Kind::Replace,
        _ => return None,
    };
    let cl_ord_id = msg.get(tags::CL_ORD_ID)?;
    issued(cl_ord_id).then(|| Request {
        seq,
        kind,
        cl_ord_id: cl_ord_id.into(),
        sent: msg.opt_field::<UtcTimestamp>(tags::SENDING_TIME).ok().flatten().map(DateTime::from),
    })
}

/// Whether every request of an order the database doesn't know was sent by `through`: the
/// order's been pruned. One without a SendingTime may be an orphan.
fn pruned(chain: &[Request], through: DateTime<Utc>) -> bool {
    chain.iter().all(|request| request.sent.is_some_and(|sent| sent <= through))
}

/// `requests` by their order's root ClOrdID, each order's in store order.
fn chains(requests: Vec<Request>) -> BTreeMap<CompactString, Vec<Request>> {
    let mut chains: BTreeMap<CompactString, Vec<Request>> = BTreeMap::new();
    for request in requests {
        let root = request.cl_ord_id.split_once('-').map_or(request.cl_ord_id.as_str(), |(root, _)| root);
        chains.entry(root.into()).or_default().push(request);
    }
    chains
}

/// Whether `id` has the shape of a downstream ClOrdID a gateway issues: `<prefix><n>` or
/// `<prefix><n>-<k>`, the prefix six base-36 digits (see [`start_prefix`]).
///
/// [`start_prefix`]: crate::book::start_prefix
fn issued(id: &str) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let root = match id.split_once('-') {
        Some((root, amendment)) if digits(amendment) => root,
        Some(_) => return false,
        None => id,
    };
    root.len() > 6
        && root.is_char_boundary(6)
        && root[..6].bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
        && digits(&root[6..])
}

/// A stored replace's terms, read as the venue's version reads a client's replace; `None`,
/// logged, if they don't read.
fn replace_terms(venue: &VenueId, msg: &Message) -> Option<OrphanedReplace> {
    let version = msg.get(tags::BEGIN_STRING).and_then(|begin_string| begin_string.parse::<Version>().ok());
    // Nobody's: only the terms are wanted.
    let nobody = ClientId::from_parts(String::new(), CompactString::default());
    match version.map(|version| version.client_input(&nobody, msg, false)) {
        Some(Ok(Input::Replace { qty, price, ord_type, time_in_force, .. })) => {
            let ord_type = ord_type.unwrap_or(if price.is_some() { OrdType::Limit } else { OrdType::Market });
            Some(OrphanedReplace { qty, price, ord_type, time_in_force })
        }
        other => {
            error!(%venue, cl_ord_id = msg.get(tags::CL_ORD_ID), ?other, "an orphaned replace's terms don't read; taken as a cancel");
            None
        }
    }
}

/// An OrderCancelRequest naming the stored order or replace `newest`, under `cl_ord_id`,
/// TransactTime to be set.
fn cancel(newest: &Message, cl_ord_id: CompactString) -> Message {
    let mut cancel = Message::new(MsgType::OrderCancelRequest)
        .with(tags::ORIG_CL_ORD_ID, newest.get(tags::CL_ORD_ID).unwrap_or_default())
        .with(tags::CL_ORD_ID, cl_ord_id);
    for tag in REPEATED {
        if let Some(value) = newest.get(tag) {
            cancel.set(tag, value);
        }
    }
    cancel
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkpoint holds only for a store created a millisecond or more before it was taken,
    /// and numbered at least up to it; a checkpoint of 0 begins at 1.
    #[test]
    fn a_scan_begins_at_the_checkpoint_only_if_the_store_wasnt_reset_since() {
        let at = |millis| DateTime::<Utc>::from_timestamp_millis(millis).expect("in range");
        let checkpoint = |next_seq| Checkpoint { next_seq, taken_at: at(1_000) };
        assert_eq!(scan_from(Some(&checkpoint(5)), Some(at(999)), 5), Ok(5));
        assert_eq!(scan_from(Some(&checkpoint(5)), Some(at(0)), 9), Ok(5));
        assert_eq!(scan_from(Some(&checkpoint(0)), Some(at(0)), 9), Ok(1));
        assert!(scan_from(None, Some(at(0)), 9).is_err());
        assert!(scan_from(Some(&checkpoint(5)), None, 9).is_err());
        // Created within the millisecond of the checkpoint, or after: reset since, perhaps.
        assert!(scan_from(Some(&checkpoint(5)), Some(at(1_000) - Duration::from_micros(999)), 9).is_err());
        assert!(scan_from(Some(&checkpoint(5)), Some(at(2_000)), 9).is_err());
        // Past the store's next number.
        assert!(scan_from(Some(&checkpoint(5)), Some(at(0)), 4).is_err());
    }

    #[test]
    fn only_ids_of_the_gateways_shapes_are_its_own() {
        for id in ["0ABCDE1", "0ABCDE12-3", "ZZZZZZ9-10"] {
            assert!(issued(id), "{id}");
        }
        for id in
            ["", "1", "1-2", "ABCDE1", "0ABCDE", "0abcde1", "0ABCDE1-", "0ABCDE1-X", "0ABCDEC1", "0ABCDE1-2-3", "C1"]
        {
            assert!(!issued(id), "{id}");
        }
    }
}
