//! Orphans: requests a venue's session stored, and so may have sent, that the database never
//! recorded, because the gateway stopped within a commit of sending them. Everything else the
//! database lacks is redelivered by the sessions, but the venue may be working an orphan with no
//! order in the book to report it to. At a start, before the venues' initiators open their
//! sessions, [`Gateway::cancel_orphans`] reads every request each venue's session stored, groups
//! them by order (the root of their ClOrdIDs), and cancels the orders of those the database has
//! never had, logging each at error:
//!
//! - an order the database never had (its NewOrderSingle orphaned, and perhaps replaces of it
//!   too) has no order in the book: an OrderCancelRequest naming the newest order or replace of
//!   it goes to the venue at its next logon, under a ClOrdID of this start's (`<prefix>C<n>`, a
//!   shape the scan ignores). The venue's answers, and any reports on the order, are logged at
//!   error, not passed to the book;
//! - an orphaned cancel or replace of an order the book holds is [`Input::Orphan`]: the venue's
//!   answers to it apply to the order, which the gateway cancels at the venue's logon.
//!
//! Each orphan is recorded once handled (see [`Changes::orphans`]), so a restart doesn't take it
//! again.
//!
//! ponytail: the whole of each venue's store is read and decoded, every start, in windows of
//! [`SCAN_WINDOW`], and its requests looked up: a million stored NewOrderSingles, all ours, took
//! 3.9 s (2.2 s reading and decoding, 1.7 s looking up), a release build on an M3 from a
//! `MemoryStorage` and an in-memory SQLite, the machine not idle. A checkpoint past what the
//! database has seen, if starts show it.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::Arc;

use tracing::{error, info, warn};
use turbojet::codec::{self, Decoded};
use turbojet::fields::{CompactString, UtcTimestamp, format_compact};
use turbojet::message::tags;
use turbojet::store::{Fetched, Opened, SentMessages, SessionLog};
use turbojet::{Message, MsgType, SessionId, SessionStorage};

use super::Gateway;
use crate::book::{Changes, ClientId, Input, OrdType, OrphanedReplace, VenueId};
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
    /// Cancels and replaces of orders the book holds, which the gateway is cancelling.
    pub amendments: usize,
}

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Order,
    Cancel,
    Replace,
}

impl Gateway {
    /// Reads every message each of `venues` (name, session and the store its initiator keeps it
    /// in) stored, and cancels the orders of the requests `store` has never recorded: see the
    /// [module documentation](self). Call it after [`restore`](Self::restore) and before the
    /// initiators start: it opens each session's log and closes it, which a `disk` store locks
    /// while open. Nothing is sent: the cancels go at each venue's logon.
    ///
    /// # Errors
    ///
    /// A venue's store or the database can't be read, or a venue holds a request under this
    /// start's prefix: the gateway started again within a second of its last start.
    pub async fn cancel_orphans(
        self: &Arc<Self>,
        store: &Store,
        venues: Vec<(String, SessionId, Arc<dyn SessionStorage>)>,
    ) -> io::Result<Orphans> {
        let mut found = Orphans::default();
        for (venue, id, storage) in venues {
            let venue: VenueId = venue.as_str().into();
            let in_store = |e: io::Error| io::Error::other(format!("venue {venue}'s store: {e}"));
            let mut log = open(storage.as_ref(), &id).await.map_err(in_store)?;
            let requests = scan(log.as_mut(), &venue, &mut found.scanned).await.map_err(in_store)?;
            self.check_prefix(&venue, &requests)?;
            let mut ids: Vec<CompactString> = requests.iter().map(|request| request.cl_ord_id.clone()).collect();
            let chains = chains(requests);
            ids.extend(chains.keys().cloned());
            let unknown: HashSet<CompactString> = store.unknown_downstreams(&ids).await?.into_iter().collect();
            for (root, chain) in chains {
                if unknown.contains(&root) {
                    let taken = self.orphaned_order(log.as_mut(), &venue, chain, found.orders).await?;
                    found.orders += usize::from(taken);
                    continue;
                }
                for request in chain.into_iter().filter(|request| unknown.contains(&request.cl_ord_id)) {
                    found.amendments += usize::from(self.orphaned_amendment(log.as_mut(), &venue, request).await?);
                }
            }
        }
        info!(
            scanned = found.scanned,
            orders = found.orders,
            amendments = found.amendments,
            "looked for venue requests the database lacks"
        );
        Ok(found)
    }

    /// Refuses a request under this start's prefix: its IDs would be this start's orders'.
    fn check_prefix(&self, venue: &VenueId, requests: &[Request]) -> io::Result<()> {
        let prefix = self.id_prefix();
        let mut ours = requests.iter().filter(|request| request.cl_ord_id.starts_with(prefix.as_str()));
        let Some(request) = ours.next().filter(|_| !prefix.is_empty()) else { return Ok(()) };
        Err(io::Error::other(format!(
            "venue {venue} holds request {} under this start's ID prefix {prefix}: the gateway started again \
             within a second of its last start; start it again",
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
            self.record_orphans(ids);
            return Ok(false);
        };
        let Some(msg) = stored(log, newest.seq).await? else { return Ok(false) };
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
        let mut state = self.lock();
        state.orphan_ids.extend(ids.iter().cloned());
        state.orphan_ids.insert(cancel.get(tags::CL_ORD_ID).unwrap_or_default().into());
        state.orphan_cancels.entry(venue.clone()).or_default().push(OrphanCancel { cancel, chain: ids });
        Ok(true)
    }

    /// Hands the book an orphaned cancel or replace of an order the database has: `true` if the
    /// book took it, and the order's cancel with it. One it doesn't is recorded, logged.
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

    /// Sends the cancels of `venue`'s orphaned orders, its session having logged on, recording
    /// each order's requests once its session has taken the cancel. One it refuses waits for the
    /// next logon; one it takes isn't watched further.
    pub(super) fn send_orphan_cancels(&self, venue: &VenueId) {
        let mut state = self.lock();
        let Some(cancels) = state.orphan_cancels.remove(venue) else { return };
        let Some(to) = state.venues.get(venue) else {
            state.orphan_cancels.insert(venue.clone(), cancels);
            return;
        };
        let now = UtcTimestamp::from(self.clock.now());
        let mut unsent = Vec::new();
        for orphan in cancels {
            let cl_ord_id = orphan.cancel.get(tags::CL_ORD_ID);
            match to.session.send(orphan.cancel.clone().with(tags::TRANSACT_TIME, now)) {
                Ok(_) => {
                    info!(%venue, cl_ord_id, chain = ?orphan.chain, "sent the cancel of an orphaned order");
                    self.record_orphans(orphan.chain);
                }
                Err(e) => {
                    error!(%venue, cl_ord_id, "cancel of an orphaned order not sent; again at the next logon: {e}");
                    unsent.push(orphan);
                }
            }
        }
        if !unsent.is_empty() {
            state.orphan_cancels.insert(venue.clone(), unsent);
        }
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
        true
    }
}

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

/// The requests of ours among every message the log stored, read [`SCAN_WINDOW`] at a time,
/// counting them into `scanned`.
async fn scan(log: &mut dyn SessionLog, venue: &VenueId, scanned: &mut usize) -> io::Result<Vec<Request>> {
    let last = log.next_outgoing().saturating_sub(1);
    let mut requests = Vec::new();
    let mut begin = 1;
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
    issued(cl_ord_id).then(|| Request { seq, kind, cl_ord_id: cl_ord_id.into() })
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
