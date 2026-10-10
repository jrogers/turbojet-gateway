//! What of the book survives a restart: the [`Changes`] its inputs make, which the store writes
//! behind the hot path, and the [`Snapshot`] the store reads back, which [`Book::restore`]
//! rebuilds a book from. Not kept: the prefix and the numbering under it (each start takes a new
//! prefix, and restored orders keep their names), what risk works out from live orders, and
//! configuration.

use std::collections::{BTreeMap, HashMap, HashSet};

use turbojet::fields::{CompactString, Decimal};

use super::{Book, ClientId, Day, Order, OrderId, Scope, Sent, Today};

/// The records the book's inputs touched since [`Book::take_changes`] last took them, as they
/// are now: whole rows, each to replace what the store has under its key. None is a delete: what
/// the book drops at a day roll stays in the store, as history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub orders: Vec<OrderRow>,
    /// What was sent to venues, by downstream ClOrdID.
    pub sent: Vec<(CompactString, Sent)>,
    pub cl_ord_ids: Vec<ClOrdIdRow>,
    pub days: Vec<(ClientId, Today)>,
    /// Every blocked scope, when the list changed, since an unblock removes one: there are few.
    pub blocked: Option<Vec<Scope>>,
    /// Reference prices, by symbol.
    pub references: Vec<(CompactString, Decimal)>,
    /// What each session has filled in its trading day: zero once a new day starts.
    pub filled: Vec<(ClientId, Decimal)>,
    /// Reports queued for clients, by row ID: the gateway's outbox adds these to the changes of
    /// the input that made them; the book never does.
    pub undelivered: Vec<(u64, Undelivered)>,
    /// Rows of `undelivered` to delete, their reports stored by the client's session or lost.
    pub delivered: Vec<u64>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A report queued for `client`, as the FIX message its session is sent, framed with the
/// clients' BeginString, BodyLength and CheckSum so [`turbojet::codec::decode`] reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undelivered {
    pub client: ClientId,
    pub message: Vec<u8>,
}

/// An order, and its client's trading day when the row was written: a done order stays in the
/// book until its client's next day. `day` is `None` for a client with no day yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRow {
    pub order: Order,
    pub day: Option<Day>,
}

/// A ClOrdID a client used for `order`, and its trading day when the row was written: a
/// ClOrdID can be used again on a later day, once its order is done. `day` is `None` for a
/// client with no day yet, and is part of the key: SQL's UNIQUE counts NULLs as distinct, so a
/// store keying on it needs a sentinel (or COALESCE) for `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClOrdIdRow {
    pub client: ClientId,
    pub day: Option<Day>,
    pub cl_ord_id: CompactString,
    pub order: OrderId,
}

/// The store's records, keyed as the store keys them, and so holding history the book has
/// dropped: [`Book::restore`] takes what the book would still hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub orders: HashMap<OrderId, OrderRow>,
    pub sent: HashMap<CompactString, Sent>,
    pub cl_ord_ids: HashMap<(ClientId, Option<Day>, CompactString), OrderId>,
    pub days: HashMap<ClientId, Today>,
    pub blocked: Vec<Scope>,
    pub references: HashMap<CompactString, Decimal>,
    pub filled: HashMap<ClientId, Decimal>,
    /// Reports not known to have reached their client, in the order they were queued.
    pub undelivered: BTreeMap<u64, Undelivered>,
    /// Past every OrderId the store has had, its history too: `MAX(id) + 1` over every order
    /// row, not just those restored. OrderIds key the orders, requests sent and ClOrdIDs, so a
    /// new order taking a dropped one's would inherit its rows at the next restart.
    pub next_order_id: u64,
}

impl Snapshot {
    /// Writes `changes` over the records, as the store does.
    pub fn apply(&mut self, changes: Changes) {
        let Changes { orders, sent, cl_ord_ids, days, blocked, references, filled, undelivered, delivered } = changes;
        for row in orders {
            self.next_order_id = self.next_order_id.max(row.order.id.0 + 1);
            self.orders.insert(row.order.id, row);
        }
        self.sent.extend(sent);
        self.cl_ord_ids.extend(cl_ord_ids.into_iter().map(|row| ((row.client, row.day, row.cl_ord_id), row.order)));
        self.days.extend(days);
        if let Some(blocked) = blocked {
            self.blocked = blocked;
        }
        self.references.extend(references);
        self.filled.extend(filled);
        self.undelivered.extend(undelivered);
        for id in delivered {
            self.undelivered.remove(&id);
        }
    }
}

/// The keys of what the book's inputs changed since the changes were last taken. Sets, so an
/// order changed many times is copied once.
#[derive(Debug, Default)]
pub(super) struct Touched {
    pub(super) orders: HashSet<OrderId>,
    pub(super) sent: HashSet<CompactString>,
    pub(super) cl_ord_ids: HashSet<(ClientId, CompactString)>,
    pub(super) days: HashSet<ClientId>,
    pub(super) blocked: bool,
    pub(super) references: HashSet<CompactString>,
    pub(super) filled: HashSet<ClientId>,
}

impl Touched {
    /// A fill on `order`: its session's filled notional, and perhaps its symbol's reference price.
    pub(super) fn fill(&mut self, order: &Order) {
        self.filled.insert(order.client.clone());
        self.references.insert(order.request.symbol.clone());
    }
}

impl Book {
    /// The records changed since the last call, copied as they are now; empty when nothing was.
    pub fn take_changes(&mut self) -> Changes {
        let mut changes = std::mem::take(&mut self.flushed);
        let days = &self.days;
        let day = |client: &ClientId| days.get(client).map(|today| today.day);
        // Drained, not taken, so the sets keep their capacity from one input to the next.
        let touched = &mut self.touched;
        for id in touched.orders.drain() {
            // A roll, the only thing that drops orders, flushes their changes first.
            let order = self.orders.get(&id).expect("orders leave the book only at a roll");
            changes.orders.push(OrderRow { order: order.clone(), day: day(&order.client) });
        }
        for downstream in touched.sent.drain() {
            let sent = self.downstream.get(&downstream).expect("requests leave the book only at a roll");
            changes.sent.push((downstream, sent.clone()));
        }
        for (client, cl_ord_id) in touched.cl_ord_ids.drain() {
            let order = *self.index.get(&(client.clone(), cl_ord_id.clone())).expect("ClOrdIDs go only at a roll");
            changes.cl_ord_ids.push(ClOrdIdRow { day: day(&client), client, cl_ord_id, order });
        }
        changes.days.extend(touched.days.drain().map(|client| {
            let today = days[&client];
            (client, today)
        }));
        if std::mem::take(&mut touched.blocked) {
            changes.blocked = Some(self.blocked.clone());
        }
        let risk = &self.risk;
        changes
            .references
            .extend(touched.references.drain().filter_map(|symbol| Some((symbol.clone(), risk.reference(&symbol)?))));
        changes.filled.extend(touched.filled.drain().map(|client| {
            let filled = risk.filled(&client);
            (client, filled)
        }));
        changes
    }

    /// Keeps the changes made so far for the next [`take_changes`](Self::take_changes), copied
    /// before a roll drops what they name and moves the days their rows are written in.
    pub(super) fn flush_changes(&mut self) {
        self.flushed = self.take_changes();
    }

    /// Fills a new book from `snapshot`: what the book held when the snapshot's last changes were
    /// taken. Set the book's prefix and configuration first, as for any new book. Risk's view of
    /// live orders is worked out again from them.
    pub fn restore(&mut self, snapshot: Snapshot) {
        assert!(self.orders.is_empty(), "restore fills a new book");
        assert_eq!(self.next_id, 1, "restore fills a new book");
        assert!(self.days.is_empty(), "restore fills a new book");
        // Undelivered reports are the gateway's to queue again, not the book's.
        let Snapshot { orders, sent, cl_ord_ids, days, blocked, references, filled, undelivered: _, next_order_id } =
            snapshot;
        for id in orders.keys() {
            assert!(id.0 < next_order_id, "the snapshot's next OrderId is past {id:?}");
        }
        self.days = days;
        let held: HashMap<OrderId, Order> = orders
            .into_values()
            .filter(|row| held(row, self.days.get(&row.order.client)))
            .map(|row| (row.order.id, row.order))
            .collect();
        self.orders = held;
        self.downstream = sent.into_iter().filter(|(_, sent)| self.orders.contains_key(&sent.order)).collect();
        for ((client, _, cl_ord_id), id) in cl_ord_ids {
            // One whose order is gone is from an earlier day, and the client may use it again.
            if self.orders.contains_key(&id) {
                self.remember(&client, cl_ord_id, id);
            }
        }
        self.blocked = blocked;
        self.risk.restore(references, filled);
        for (id, order) in &self.orders {
            self.risk.track(*id, order.exposure());
        }
        self.next_id = next_order_id.max(1);
        self.touched = Touched::default();
        // Once a start, so in release builds too: a store that gave back something the book
        // couldn't have held stops the gateway before it trades. O(clients x ClOrdIDs).
        self.check();
    }

    /// The book's records, written now: what a store of every change taken, restored, gives back.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Snapshot {
        let day = |client: &ClientId| self.days.get(client).map(|today| today.day);
        Snapshot {
            orders: self
                .orders
                .iter()
                .map(|(id, order)| (*id, OrderRow { order: order.clone(), day: day(&order.client) }))
                .collect(),
            sent: self.downstream.clone(),
            cl_ord_ids: self
                .index
                .iter()
                .map(|((client, cl_ord_id), id)| ((client.clone(), day(client), cl_ord_id.clone()), *id))
                .collect(),
            days: self.days.clone(),
            blocked: self.blocked.clone(),
            references: self.risk.references().clone(),
            filled: self.risk.filled_by_client().collect(),
            undelivered: BTreeMap::new(),
            next_order_id: self.next_id,
        }
    }
}

/// Whether the book holds `row`'s order, its client's day being `today`: a live order always, a
/// done one until its client's next trading day, whose roll drops it.
fn held(row: &OrderRow, today: Option<&Today>) -> bool {
    if !row.order.status.is_done() {
        return true;
    }
    match (row.day, today) {
        (Some(day), Some(today)) => day == today.day,
        // Written before the client's first day: a roll since has dropped it.
        (None, Some(today)) => today.began.is_none(),
        (_, None) => {
            assert!(row.day.is_none(), "a row's day is its client's");
            true
        }
    }
}
