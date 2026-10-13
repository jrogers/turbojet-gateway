//! What the book shows an operator: owned views of its orders and risk, built under the gateway's
//! lock from the book's own maps, without copying them.

use std::collections::{BTreeMap, BinaryHeap};

use chrono::{DateTime, Utc};
use serde::Serialize;
use turbojet::fields::{CompactString, Decimal, format_compact};

use super::{
    Book, CancelKind, ClientId, GatewayCancel, OrdType, Order, OrderId, Origin, Scope, Side, Status, VenueId,
    limit_price,
};
use crate::risk::Limits;

/// Orders one call to [`Book::orders`] returns at most: a blotter page. Each view is a few hundred
/// bytes, made under the gateway's lock, so a page bounds both the views and what the walk keeps;
/// the rest are a page away (`after`).
pub const MAX_ORDERS_PER_PAGE: usize = 1_000;

/// Digits in the largest `u64`, so in any OrderId: what [`Book::order_named`] tries at most.
const MAX_ORDER_ID_DIGITS: usize = 20;

/// Which orders [`Book::orders`] returns: those matching every field set, by OrderId.
#[derive(Debug, Clone, Default)]
pub struct OrderFilter {
    /// A client session, or every session of a CompID.
    pub client: Option<Scope>,
    pub venue: Option<VenueId>,
    pub symbol: Option<CompactString>,
    pub status: Option<Status>,
    /// Only orders not yet done.
    pub working: bool,
    /// Only orders after this one: the last of the previous page.
    pub after: Option<OrderId>,
    /// At most this many, and never more than [`MAX_ORDERS_PER_PAGE`]; `None` is that many.
    pub limit: Option<usize>,
}

impl OrderFilter {
    fn matches(&self, order: &Order) -> bool {
        self.after.is_none_or(|after| order.id.0 > after.0)
            && self.client.as_ref().is_none_or(|scope| scope.covers(&order.client))
            && self.venue.as_ref().is_none_or(|venue| order.venue == *venue)
            && self.symbol.as_ref().is_none_or(|symbol| order.request.symbol == *symbol)
            && self.status.is_none_or(|status| order.status == status)
            && !(self.working && order.status.is_done())
    }
}

/// An order as a blotter shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderView {
    /// The book's key: the API's paging cursor ([`OrderFilter::after`]), as a number. `order_id`
    /// is what the wire knows.
    #[serde(serialize_with = "order_id_number")]
    pub id: OrderId,
    /// Our OrderID(37): [`Order::name`].
    pub order_id: CompactString,
    pub client: ClientId,
    /// The client's current ClOrdID.
    pub cl_ord_id: CompactString,
    pub venue: VenueId,
    pub symbol: CompactString,
    pub side: Side,
    pub ord_type: OrdType,
    pub qty: Decimal,
    /// The limit price; `None` for a Market order.
    pub price: Option<Decimal>,
    pub cum_qty: Decimal,
    pub avg_px: Decimal,
    pub status: Status,
    /// A cancel, the client's or the gateway's, that the venue hasn't answered.
    pub pending_cancel: bool,
    /// A replace the venue hasn't answered.
    pub pending_replace: bool,
    pub gateway_cancel: Option<GatewayCancelView>,
    /// A venue said it doesn't know the order; an operator may resolve it.
    pub unknown_at_venue: bool,
    /// When the order was sent to its venue.
    pub created: DateTime<Utc>,
}

/// An order with every request sent for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderDetail {
    #[serde(flatten)]
    pub order: OrderView,
    /// The venue's OrderID(37), once a report has given it.
    pub venue_order_id: Option<CompactString>,
    /// In the order they were sent.
    pub requests: Vec<RequestView>,
}

/// A request sent to the order's venue: see [`Sent`](super::Sent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestView {
    /// The ClOrdID the venue knows it by.
    pub downstream: CompactString,
    pub kind: RequestKind,
    pub origin: Origin,
    /// The client's ClOrdID for it; `None` for the gateway's own.
    pub cl_ord_id: Option<CompactString>,
    pub sent_at: DateTime<Utc>,
    /// The venue has rejected or accepted it.
    pub answered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    New,
    Cancel,
    Replace,
}

/// The gateway's own cancel of an order: see [`GatewayCancel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GatewayCancelView {
    pub state: CancelState,
    pub tries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelState {
    /// Waiting to be sent: the venue's session couldn't take it, or a client's cancel is ahead.
    Waiting,
    InFlight,
    /// Given up: someone has to look, and may cancel the order again for another round.
    Stuck,
}

impl From<&GatewayCancel> for GatewayCancelView {
    fn from(cancel: &GatewayCancel) -> Self {
        let state = match (cancel.stuck, cancel.unsent) {
            (true, _) => CancelState::Stuck,
            (false, true) => CancelState::Waiting,
            (false, false) => CancelState::InFlight,
        };
        Self { state, tries: cancel.tries }
    }
}

/// The limits in force and what each session counts against them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RiskView {
    pub defaults: Limits,
    pub by_comp_id: BTreeMap<CompactString, Limits>,
    /// By session.
    pub sessions: Vec<SessionRisk>,
    /// Each symbol's last fill price.
    pub references: BTreeMap<CompactString, Decimal>,
    /// The scopes blocked, in the order they were blocked.
    pub blocked: Vec<Scope>,
}

/// A session's open notional (its live orders') and what it has filled in its trading day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRisk {
    pub client: ClientId,
    pub open: Decimal,
    pub filled: Decimal,
}

impl Book {
    /// The orders matching `filter`, by OrderId. The walk is of the whole book, O(book) under the
    /// gateway's lock; the page's limit bounds what it keeps and the views it makes.
    // ponytail: a walk per page; an index of OrderIds in order (a BTreeMap) gives `range(after..)`
    // at O(page) if blotter calls show up in the gateway's latency.
    pub fn orders(&self, filter: &OrderFilter) -> Vec<OrderView> {
        let limit = filter.limit.unwrap_or(MAX_ORDERS_PER_PAGE).min(MAX_ORDERS_PER_PAGE);
        // The lowest `limit` IDs so far, the highest on top to be pushed out.
        let mut lowest = BinaryHeap::with_capacity(limit.saturating_add(1));
        for order in self.orders.values().filter(|order| filter.matches(order)) {
            lowest.push(order.id.0);
            if lowest.len() > limit {
                lowest.pop();
            }
        }
        lowest.into_sorted_vec().into_iter().map(|id| self.view(&self.orders[&OrderId(id)])).collect()
    }

    /// The order and every request sent for it, if the book holds it.
    pub fn order_detail(&self, id: OrderId) -> Option<OrderDetail> {
        let order = self.orders.get(&id)?;
        // Its requests are `<name>` and `<name>-<n>`, numbered by `amendments`: no walk of them
        // all.
        let names = std::iter::once(order.name.clone())
            .chain((1..=order.amendments).map(|n| format_compact!("{}-{n}", order.name)));
        let requests = names
            .filter_map(|downstream| {
                let sent = self.downstream.get(&downstream)?;
                let kind = match sent.kind {
                    None => RequestKind::New,
                    Some(CancelKind::Cancel) => RequestKind::Cancel,
                    Some(CancelKind::Replace) => RequestKind::Replace,
                };
                let cl_ord_id = (sent.origin == Origin::Client).then(|| sent.cl_ord_id.clone());
                let (origin, sent_at, answered) = (sent.origin, sent.sent_at, sent.answered);
                Some(RequestView { downstream, kind, origin, cl_ord_id, sent_at, answered })
            })
            .collect();
        Some(OrderDetail { order: self.view(order), venue_order_id: order.venue_order_id.clone(), requests })
    }

    /// The order whose OrderID(37) is `name`, if the book holds it. Restored orders keep an
    /// earlier start's prefix, so the name's trailing digits are tried as each possible number,
    /// a handful of lookups, rather than stripping this start's prefix.
    pub fn order_named(&self, name: &str) -> Option<OrderId> {
        let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let digits = digits.min(MAX_ORDER_ID_DIGITS);
        (1..=digits).find_map(|len| {
            let id = OrderId(name[name.len() - len..].parse().ok()?);
            self.orders.get(&id).filter(|order| order.name == name).map(|order| order.id)
        })
    }

    /// The order the client's session knows by `cl_ord_id`, if the book holds it: any ClOrdID the
    /// order went by today.
    pub fn order_by_cl_ord_id(&self, client: &ClientId, cl_ord_id: &str) -> Option<OrderId> {
        self.index.get(&(client.clone(), cl_ord_id.into())).copied()
    }

    /// The limits in force, each session's notionals, the reference prices and the blocks.
    pub fn risk_view(&self) -> RiskView {
        let mut sessions: Vec<SessionRisk> = self
            .risk
            .sessions()
            .map(|(client, open, filled)| SessionRisk { client: client.clone(), open, filled })
            .collect();
        sessions.sort_unstable_by(|a, b| a.client.cmp(&b.client));
        RiskView {
            defaults: self.limits.defaults.clone(),
            by_comp_id: self
                .limits
                .by_comp_id
                .iter()
                .map(|(comp_id, limits)| (comp_id.clone(), limits.clone()))
                .collect(),
            sessions,
            references: self.risk.references().iter().map(|(symbol, price)| (symbol.clone(), *price)).collect(),
            blocked: self.blocked.clone(),
        }
    }

    fn view(&self, order: &Order) -> OrderView {
        OrderView {
            id: order.id,
            order_id: order.name.clone(),
            client: order.client.clone(),
            cl_ord_id: order.request.cl_ord_id.clone(),
            venue: order.venue.clone(),
            symbol: order.request.symbol.clone(),
            side: order.request.side,
            ord_type: order.request.ord_type,
            qty: order.request.qty,
            price: limit_price(order.request.ord_type, order.request.price),
            cum_qty: order.cum_qty,
            avg_px: order.avg_px,
            status: order.status,
            pending_cancel: self.outstanding_cancel(order).is_some(),
            pending_replace: order.pending.is_some(),
            gateway_cancel: order.gateway_cancel.as_ref().map(GatewayCancelView::from),
            unknown_at_venue: self.unknown_at_venue.contains(&order.id),
            created: self.downstream.get(&order.name).expect("an order's original request is recorded").sent_at,
        }
    }
}

fn order_id_number<S: serde::Serializer>(id: &OrderId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(id.0)
}

/// What the gateway tells whoever listens (see `Gateway::subscribe`), as JSON tagged by `type`:
/// `{"type":"order", ...}`, `{"type":"blocked","scope":"all"}`, `{"type":"alarm", ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayEvent {
    /// An order as it is after a step that changed it.
    Order(OrderView),
    /// Struct variants, not `Blocked(Scope)`: a tag can't go inside a string.
    Blocked {
        scope: Scope,
    },
    Unblocked {
        scope: Scope,
    },
    Alarm(Alarm),
}

/// Something an operator should look at; logged as well.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alarm {
    pub time: DateTime<Utc>,
    pub kind: AlarmKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlarmKind {
    /// The database is too far behind: new orders are rejected.
    GatewayUnavailable,
    /// The database has caught up after [`GatewayUnavailable`](Self::GatewayUnavailable): new
    /// orders are taken again.
    GatewayAvailable,
    /// The gateway gave up cancelling an order.
    StuckCancel,
    /// A venue holds a request the database never had, or reports on one.
    Orphan,
    /// A venue reported more filled than the order's quantity.
    Overfill,
    /// A venue reported on an order the book doesn't have.
    UnknownOrder,
    /// A venue answered a status request saying it doesn't know an order the book has live.
    UnknownAtVenue,
    /// A venue reported a fill on an order the book has as done: not applied; someone has to
    /// settle it with the venue.
    FillOnDoneOrder,
    /// A venue's session reset its store, so requests it never stored failed rather than go again.
    VenueStoreReset,
}

impl Book {
    /// The orders touched since the changes were last taken, by OrderId, as they are now: those a
    /// roll dropped aren't, as they were done already and shown so before it.
    pub fn touched_views(&self) -> Vec<OrderView> {
        let flushed = self.flushed.orders.iter().map(|row| &row.order.id);
        let mut ids: Vec<OrderId> = self.touched.orders.iter().chain(flushed).copied().collect();
        ids.sort_unstable_by_key(|id| id.0);
        ids.dedup();
        ids.iter().filter_map(|id| self.orders.get(id)).map(|order| self.view(order)).collect()
    }

    /// Events the book's inputs made since the last call: blocks, unblocks and alarms.
    pub fn take_events(&mut self) -> std::vec::Drain<'_, GatewayEvent> {
        self.events.drain(..)
    }

    /// Records an alarm of `kind` at the inputs' time, for [`take_events`](Self::take_events).
    pub(super) fn alarm(&mut self, kind: AlarmKind, text: String) {
        self.events.push(GatewayEvent::Alarm(Alarm { time: self.now, kind, text }));
    }
}
