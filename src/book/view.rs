//! What the book shows an operator: owned views of its orders and risk, built under the gateway's
//! lock from the book's own maps, without copying them.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use turbojet::fields::{CompactString, Decimal, format_compact};

use super::{
    Book, CancelKind, ClientId, GatewayCancel, OrdType, Order, OrderId, Origin, Scope, Side, Status, VenueId,
    limit_price,
};
use crate::risk::Limits;

/// Orders one call to [`Book::orders`] returns at most: a blotter page. Each view is a few hundred
/// bytes, and the walk that finds them holds the gateway's lock, so a page stays small; the rest
/// are a page away (`after`).
pub const MAX_ORDERS_PER_PAGE: usize = 1_000;

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
    /// The book's key, for paging ([`OrderFilter::after`]); `order_id` is what the wire knows.
    #[serde(skip)]
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
    /// When the order was sent to its venue; `None` if the book no longer has that request.
    pub created: Option<DateTime<Utc>>,
}

/// An order with every request sent for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderDetail {
    #[serde(flatten)]
    pub order: OrderView,
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
    /// The orders matching `filter`, by OrderId: one walk of the book, the views made only for the
    /// page.
    pub fn orders(&self, filter: &OrderFilter) -> Vec<OrderView> {
        let limit = filter.limit.unwrap_or(MAX_ORDERS_PER_PAGE).min(MAX_ORDERS_PER_PAGE);
        let mut page: Vec<&Order> = self.orders.values().filter(|order| filter.matches(order)).collect();
        if page.len() > limit {
            // The lowest `limit` without sorting every match.
            page.select_nth_unstable_by_key(limit, |order| order.id.0);
            page.truncate(limit);
        }
        page.sort_unstable_by_key(|order| order.id.0);
        page.into_iter().map(|order| self.view(order)).collect()
    }

    /// The order and every request sent for it, if the book holds it.
    pub fn order_detail(&self, id: OrderId) -> Option<OrderDetail> {
        let order = self.orders.get(&id)?;
        // Its requests are `<name>` and `<name>-<n>`, numbered by `amendments`: no walk of them all.
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
        Some(OrderDetail { order: self.view(order), requests })
    }

    /// The order whose OrderID(37) is `name`, if the book holds it. Restored orders keep an
    /// earlier start's prefix, so the name's trailing digits are tried as each possible number,
    /// a handful of lookups, rather than stripping this start's prefix.
    pub fn order_named(&self, name: &str) -> Option<OrderId> {
        let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
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
            created: self.downstream.get(&order.name).map(|sent| sent.sent_at),
        }
    }
}
