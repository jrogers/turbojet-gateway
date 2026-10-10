//! The order book: a sans-IO state machine. Each [`Input`] changes the book and pushes what to
//! send onto `out`; the FIX side turns those into typed messages for each session's version.

use std::collections::HashMap;

use turbojet::fields::{CompactString, Decimal, format_compact};

use crate::router::{Router, Target};

/// A client session, by the client's SenderCompID.
pub type ClientId = CompactString;
/// A venue session, by its name in the sessions file.
pub type VenueId = CompactString;

/// ClOrdIDs a client may use in one trading day, originals and amendments together. The index
/// is in memory, so it has a bound; a busy client sends tens of thousands of orders a day.
pub const MAX_CL_ORD_IDS_PER_CLIENT: usize = 1_000_000;

/// Our OrderID(37), and the root of every downstream ClOrdID for the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
    SellShort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdType {
    Market,
    Limit,
}

/// An order as the client sent it, whatever its FIX version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    pub cl_ord_id: CompactString,
    pub account: Option<CompactString>,
    pub symbol: CompactString,
    pub side: Side,
    pub ord_type: OrdType,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    pub ex_destination: Option<CompactString>,
    pub time_in_force: Option<TimeInForce>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    Day,
    Gtc,
    Ioc,
    Fok,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    PendingNew,
    New,
    PartiallyFilled,
    Filled,
    PendingCancel,
    PendingReplace,
    Canceled,
    Expired,
    Rejected,
}

impl Status {
    fn is_done(self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Expired | Self::Rejected)
    }
}

/// Why we rejected an order ourselves; the FIX side maps it to OrdRejReason(103).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Duplicate,
    NoRoute,
    TooManyOrders,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub client: ClientId,
    pub venue: VenueId,
    /// As last accepted: `cl_ord_id` is the client's current ClOrdID.
    pub request: OrderRequest,
    /// The ClOrdID the venue knows the order by now.
    pub downstream: CompactString,
    /// Cancels and replaces sent so far, numbering the next downstream ClOrdID.
    pub amendments: u32,
    pub status: Status,
    pub cum_qty: Decimal,
    pub avg_px: Decimal,
}

/// An ExecutionReport from a venue, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueReport {
    /// The downstream ClOrdID it answers.
    pub downstream: CompactString,
    pub status: Status,
    pub last_qty: Decimal,
    pub last_px: Decimal,
    pub cum_qty: Decimal,
    pub avg_px: Decimal,
    pub text: Option<CompactString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// `redelivered`: Turbojet's `maybe_redelivered()`, PossResend(97) or PossDupFlag(43).
    NewOrder {
        client: ClientId,
        request: OrderRequest,
        redelivered: bool,
    },
    Cancel {
        client: ClientId,
        cl_ord_id: CompactString,
        orig_cl_ord_id: CompactString,
    },
    VenueReport {
        venue: VenueId,
        report: VenueReport,
    },
    VenueCancelReject {
        venue: VenueId,
        downstream: CompactString,
        text: Option<CompactString>,
    },
}

/// An ExecutionReport for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientReport {
    /// `None` for an order rejected before it was booked.
    pub order_id: Option<OrderId>,
    pub cl_ord_id: CompactString,
    pub orig_cl_ord_id: Option<CompactString>,
    pub request: OrderRequest,
    pub status: Status,
    pub reject: Option<RejectReason>,
    pub last_qty: Decimal,
    pub last_px: Decimal,
    pub cum_qty: Decimal,
    pub leaves_qty: Decimal,
    pub avg_px: Decimal,
    pub text: Option<CompactString>,
}

/// What an OrderCancelReject answers: ResponseTo(434).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelKind {
    Cancel,
    Replace,
}

/// CxlRejReason(102): 0, 1, 6 and 99; the FIX side maps it per version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRejectReason {
    TooLate,
    UnknownOrder,
    Duplicate,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Report {
        client: ClientId,
        report: ClientReport,
    },
    CancelReject {
        client: ClientId,
        order_id: Option<OrderId>,
        cl_ord_id: CompactString,
        orig_cl_ord_id: CompactString,
        response_to: CancelKind,
        /// The order's status, `Rejected` when there is no order.
        status: Status,
        reason: CancelRejectReason,
        text: CompactString,
    },
    NewOrder {
        venue: VenueId,
        downstream: CompactString,
        request: OrderRequest,
    },
    Cancel {
        venue: VenueId,
        downstream: CompactString,
        orig_downstream: CompactString,
        request: OrderRequest,
    },
}

#[derive(Debug)]
pub struct Book {
    router: Router,
    orders: HashMap<OrderId, Order>,
    /// (client, ClOrdID) -> order, for every ClOrdID each client used today.
    index: HashMap<(ClientId, CompactString), OrderId>,
    /// ClOrdIDs in `index` per client, against [`MAX_CL_ORD_IDS_PER_CLIENT`].
    used: HashMap<ClientId, usize>,
    /// Downstream ClOrdID -> the order, and the client's ClOrdID for that request.
    downstream: HashMap<CompactString, (OrderId, CompactString)>,
    next_id: u64,
}

impl Book {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            orders: HashMap::new(),
            index: HashMap::new(),
            used: HashMap::new(),
            downstream: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    pub fn handle(&mut self, input: Input, out: &mut Vec<Output>) {
        match input {
            Input::NewOrder { client, request, redelivered } => self.new_order(client, request, redelivered, out),
            Input::Cancel { .. } | Input::VenueReport { .. } | Input::VenueCancelReject { .. } => todo!(),
        }
    }

    fn new_order(&mut self, client: ClientId, request: OrderRequest, _redelivered: bool, out: &mut Vec<Output>) {
        let venue = match self.router.route(&client, &request) {
            Target::Venue(venue) => venue,
            Target::Reject(text) => return out.push(reject(client, request, RejectReason::NoRoute, text)),
        };
        let id = OrderId(self.next_id);
        self.next_id += 1;
        let downstream = format_compact!("{}", id.0);
        self.remember(&client, request.cl_ord_id.clone(), id);
        self.downstream.insert(downstream.clone(), (id, request.cl_ord_id.clone()));
        out.push(Output::NewOrder { venue: venue.clone(), downstream: downstream.clone(), request: request.clone() });
        let order = Order {
            id,
            client,
            venue,
            request,
            downstream,
            amendments: 0,
            status: Status::PendingNew,
            cum_qty: Decimal::ZERO,
            avg_px: Decimal::ZERO,
        };
        self.orders.insert(id, order);
    }

    fn remember(&mut self, client: &ClientId, cl_ord_id: CompactString, id: OrderId) {
        self.index.insert((client.clone(), cl_ord_id), id);
        *self.used.entry(client.clone()).or_default() += 1;
    }
}

fn reject(client: ClientId, request: OrderRequest, reason: RejectReason, text: CompactString) -> Output {
    let report = ClientReport {
        order_id: None,
        cl_ord_id: request.cl_ord_id.clone(),
        orig_cl_ord_id: None,
        request,
        status: Status::Rejected,
        reject: Some(reason),
        last_qty: Decimal::ZERO,
        last_px: Decimal::ZERO,
        cum_qty: Decimal::ZERO,
        leaves_qty: Decimal::ZERO,
        avg_px: Decimal::ZERO,
        text: Some(text),
    };
    Output::Report { client, report }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Rule;

    fn book() -> Book {
        Book::new(Router::new(vec![Rule {
            client: None,
            ex_destination: Some("XNAS".into()),
            symbol: None,
            target: Target::Venue("NASDAQ".into()),
        }]))
    }

    fn request(cl_ord_id: &str) -> OrderRequest {
        OrderRequest {
            cl_ord_id: cl_ord_id.into(),
            account: None,
            symbol: "AAPL".into(),
            side: Side::Buy,
            ord_type: OrdType::Limit,
            qty: Decimal::from(100),
            price: Some(Decimal::from(10)),
            ex_destination: Some("XNAS".into()),
            time_in_force: None,
        }
    }

    fn run(book: &mut Book, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        book.handle(input, &mut out);
        out
    }

    fn new_order(book: &mut Book, cl_ord_id: &str) -> Vec<Output> {
        run(book, Input::NewOrder { client: "FUNDA".into(), request: request(cl_ord_id), redelivered: false })
    }

    #[test]
    fn a_new_order_goes_to_its_venue_under_our_order_id() {
        let mut book = book();
        let out = new_order(&mut book, "C1");
        assert_eq!(out, [Output::NewOrder { venue: "NASDAQ".into(), downstream: "1".into(), request: request("C1") }]);
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingNew);
    }

    #[test]
    fn an_unroutable_order_is_rejected_and_not_booked() {
        let mut book = book();
        let mut req = request("C1");
        req.ex_destination = None;
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: req, redelivered: false });
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(
            (report.status, report.reject, report.order_id),
            (Status::Rejected, Some(RejectReason::NoRoute), None)
        );
        assert!(book.order(OrderId(1)).is_none());
    }
}
