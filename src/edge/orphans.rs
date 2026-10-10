//! Orphans: requests a venue's session stored, and so may have sent, that the database never
//! recorded, because the gateway stopped within a commit of sending them. Everything else the
//! database lacks is redelivered by the sessions, but the venue may be working an orphan with no
//! order in the book to report it to. At a start, before the venues' initiators open their
//! sessions, [`Gateway::cancel_orphans`] reads every request each venue's session stored and
//! cancels the orders of those the database has never had, logging each at error:
//!
//! - an orphaned NewOrderSingle has no order in the book: an OrderCancelRequest built from it
//!   goes to the venue at its next logon, under a ClOrdID of this start's (`<prefix>C<n>`, a
//!   shape the scan ignores). The venue's answers, and any fills, are reports for an unknown
//!   order, which the book logs and drops. It's cancelled again at each start until the venue's
//!   session drops it from its store (a sequence reset);
//! - an orphaned cancel or replace of an order the book holds is [`Input::Orphan`]: the venue's
//!   answers to it apply to the order, which the gateway cancels.
//!
//! ponytail: the whole of each venue's store is read and decoded, every start: a few seconds for
//! a million stored messages. A checkpoint past what the database has seen, if starts show it.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use tracing::{error, info};
use turbojet::fields::{CompactString, UtcTimestamp, format_compact};
use turbojet::message::tags;
use turbojet::store::{Fetched, Opened, SentMessages};
use turbojet::{Message, MsgType, SessionId, SessionStorage};

use super::Gateway;
use crate::book::{Input, VenueId};
use crate::store::Store;

/// What [`Gateway::cancel_orphans`] found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Orphans {
    /// Messages read from the venues' stores.
    pub scanned: usize,
    /// NewOrderSingles whose cancel waits for the venue's logon.
    pub new_orders: usize,
    /// Cancels and replaces whose orders the gateway is cancelling.
    pub amendments: usize,
}

/// The fields of an order its cancel repeats, as FIX asks: the order's own values, in the venue's
/// version, since the cancel is built from the order as stored.
const REPEATED: [u32; 4] = [tags::ACCOUNT, tags::SYMBOL, tags::SIDE, tags::ORDER_QTY];

impl Gateway {
    /// Reads every message each of `venues` (name, session and the store its initiator keeps it
    /// in) stored, and cancels the orders of the requests `store` has never recorded: see the
    /// [module documentation](self). Call it after [`restore`](Self::restore) and before the
    /// initiators start: it opens each session's log and closes it, which a `disk` store locks
    /// while open.
    ///
    /// # Errors
    ///
    /// A venue's store or the database can't be read.
    pub async fn cancel_orphans(
        self: &Arc<Self>,
        store: &Store,
        venues: Vec<(String, SessionId, Arc<dyn SessionStorage>)>,
    ) -> io::Result<Orphans> {
        let mut found = Orphans::default();
        let mut requests: Vec<(VenueId, Message)> = Vec::new();
        for (venue, id, storage) in venues {
            let stored = stored(storage.as_ref(), &id)
                .await
                .map_err(|e| io::Error::other(format!("venue {venue:?}'s store: {e}")))?;
            found.scanned += stored.len();
            requests.extend(stored.iter().filter_map(|(_, bytes)| ours(bytes)).map(|msg| (venue.as_str().into(), msg)));
        }
        let ids: Vec<CompactString> = requests.iter().map(|(_, msg)| cl_ord_id(msg).into()).collect();
        let unknown: HashSet<CompactString> = store.unknown_downstreams(&ids).await?.into_iter().collect();
        let prefix = self.id_prefix();
        for (venue, msg) in requests.into_iter().filter(|(_, msg)| unknown.contains(cl_ord_id(msg))) {
            if msg.msg_type() == MsgType::NewOrderSingle {
                found.new_orders += 1;
                let cancel = cancel(&msg, format_compact!("{prefix}C{}", found.new_orders));
                error!(
                    %venue,
                    cl_ord_id = cl_ord_id(&msg),
                    account = msg.get(tags::ACCOUNT),
                    symbol = msg.get(tags::SYMBOL),
                    side = msg.get(tags::SIDE),
                    qty = msg.get(tags::ORDER_QTY),
                    price = msg.get(tags::PRICE),
                    cancel = cancel.get(tags::CL_ORD_ID),
                    "venue holds an order the database lacks: cancelling it at the venue's logon"
                );
                self.lock().orphan_cancels.entry(venue).or_default().push(cancel);
            } else {
                found.amendments += 1;
                self.handle(Input::Orphan { venue, downstream: cl_ord_id(&msg).into() });
            }
        }
        info!(
            scanned = found.scanned,
            new_orders = found.new_orders,
            amendments = found.amendments,
            "looked for venue requests the database lacks"
        );
        Ok(found)
    }

    /// Sends the cancels of `venue`'s orphaned orders, its session having logged on. One its
    /// session refuses waits for the next logon; one it takes isn't watched further.
    pub(super) fn send_orphan_cancels(&self, venue: &VenueId) {
        let mut state = self.lock();
        let Some(cancels) = state.orphan_cancels.remove(venue) else { return };
        let Some(to) = state.venues.get(venue) else {
            state.orphan_cancels.insert(venue.clone(), cancels);
            return;
        };
        let now = UtcTimestamp::from(self.clock.now());
        let mut unsent = Vec::new();
        for cancel in cancels {
            match to.session.send(cancel.clone().with(tags::TRANSACT_TIME, now)) {
                Ok(_) => info!(%venue, cl_ord_id = cancel.get(tags::CL_ORD_ID), "sent the cancel of an orphaned order"),
                Err(e) => {
                    error!(%venue, cl_ord_id = cancel.get(tags::CL_ORD_ID), "cancel of an orphaned order not sent; again at the next logon: {e}");
                    unsent.push(cancel);
                }
            }
        }
        if !unsent.is_empty() {
            state.orphan_cancels.insert(venue.clone(), unsent);
        }
    }
}

/// Every message session `id` stored in `storage`, its log closed again.
async fn stored(storage: &dyn SessionStorage, id: &SessionId) -> io::Result<SentMessages> {
    let mut log = match storage.begin_open(id)? {
        Opened::Ready(log) => log,
        Opened::Pending(job) => job.run_async().await?,
    };
    let last = log.next_outgoing().saturating_sub(1);
    if last == 0 {
        return Ok(SentMessages::new());
    }
    match log.fetch(1, last)? {
        Fetched::Ready(stored) => Ok(stored),
        Fetched::Pending(job) => job.run_async().await,
    }
}

/// The stored message, if it's a NewOrderSingle, OrderCancelRequest or OrderCancelReplaceRequest
/// under a ClOrdID of a shape this gateway issues.
fn ours(bytes: &[u8]) -> Option<Message> {
    let turbojet::codec::Decoded::Message(msg, _) = turbojet::codec::decode(bytes) else { return None };
    let request = matches!(
        msg.msg_type(),
        MsgType::NewOrderSingle | MsgType::OrderCancelRequest | MsgType::OrderCancelReplaceRequest
    );
    (request && issued(cl_ord_id(&msg))).then_some(msg)
}

fn cl_ord_id(msg: &Message) -> &str {
    msg.get(tags::CL_ORD_ID).unwrap_or_default()
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

/// An OrderCancelRequest for the stored `order`, under `cl_ord_id`, TransactTime to be set.
fn cancel(order: &Message, cl_ord_id: CompactString) -> Message {
    let mut cancel = Message::new(MsgType::OrderCancelRequest)
        .with(tags::ORIG_CL_ORD_ID, self::cl_ord_id(order))
        .with(tags::CL_ORD_ID, cl_ord_id);
    for tag in REPEATED {
        if let Some(value) = order.get(tag) {
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
