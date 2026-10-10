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

/// What a [`ClientReport`] tells the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// Something happened to the order: an ack, a fill, a cancel, a reject.
    Event,
    /// An answer to a redelivery, nothing new: sent as ExecType=I (4.3+) or ExecTransType=3 (4.2).
    Status,
}

/// An ExecutionReport for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientReport {
    pub kind: ReportKind,
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
            Input::VenueReport { venue, report } => self.venue_report(&venue, report, out),
            Input::Cancel { .. } | Input::VenueCancelReject { .. } => todo!(),
        }
    }

    fn new_order(&mut self, client: ClientId, request: OrderRequest, redelivered: bool, out: &mut Vec<Output>) {
        if let Some(&id) = self.index.get(&(client.clone(), request.cl_ord_id.clone())) {
            let order = self.orders.get(&id).expect("indexed ClOrdIDs point at booked orders");
            // A resend of an order we booked: answer with where it stands. Side, symbol and account,
            // not the whole request, since a replace may have changed quantity and price since.
            let resent = redelivered
                && order.request.side == request.side
                && order.request.symbol == request.symbol
                && order.request.account == request.account;
            let output = if resent {
                Output::Report { client, report: order.report(&request.cl_ord_id, ReportKind::Status) }
            } else {
                reject(client, request, RejectReason::Duplicate, "duplicate ClOrdID".into())
            };
            return out.push(output);
        }
        if self.used.get(&client).is_some_and(|&used| used >= MAX_CL_ORD_IDS_PER_CLIENT) {
            return out.push(reject(client, request, RejectReason::TooManyOrders, "too many orders today".into()));
        }
        let venue = match self.router.route(&client, &request) {
            Target::Venue(venue) => venue,
            Target::Reject(text) => return out.push(reject(client, request, RejectReason::NoRoute, text)),
        };
        let id = OrderId(self.next_id);
        self.next_id += 1;
        let downstream = format_compact!("{}", id.0);
        self.remember(&client, request.cl_ord_id.clone(), id);
        let previous = self.downstream.insert(downstream.clone(), (id, request.cl_ord_id.clone()));
        debug_assert!(previous.is_none(), "downstream ClOrdID {downstream} reused");
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
        let previous = self.orders.insert(id, order);
        debug_assert!(previous.is_none(), "order {id:?} booked twice");
    }

    fn venue_report(&mut self, venue: &VenueId, report: VenueReport, out: &mut Vec<Output>) {
        let Some((id, cl_ord_id)) = self.downstream.get(&report.downstream).cloned() else {
            return tracing::warn!(%venue, downstream = %report.downstream, "execution report for an unknown order");
        };
        let order = self.orders.get_mut(&id).expect("downstream ClOrdIDs point at booked orders");
        if order.venue != *venue {
            return tracing::warn!(%venue, downstream = %report.downstream, "execution report for another venue's order");
        }
        if stale(order, &report) {
            return tracing::debug!(%venue, downstream = %report.downstream, "stale execution report dropped");
        }
        if report.cum_qty > order.request.qty {
            return tracing::warn!(%venue, downstream = %report.downstream, cum_qty = %report.cum_qty, "overfill");
        }
        let orig_cl_ord_id = (cl_ord_id != order.request.cl_ord_id).then(|| order.request.cl_ord_id.clone());
        order.status = report.status;
        order.cum_qty = report.cum_qty;
        order.avg_px = report.avg_px;
        let mut client_report = order.report(&cl_ord_id, ReportKind::Event);
        client_report.orig_cl_ord_id = orig_cl_ord_id;
        client_report.last_qty = report.last_qty;
        client_report.last_px = report.last_px;
        client_report.text = report.text;
        out.push(Output::Report { client: order.client.clone(), report: client_report });
    }

    fn remember(&mut self, client: &ClientId, cl_ord_id: CompactString, id: OrderId) {
        let previous = self.index.insert((client.clone(), cl_ord_id), id);
        debug_assert!(previous.is_none(), "ClOrdID reused by {client}");
        *self.used.entry(client.clone()).or_default() += 1;
    }
}

impl Order {
    /// A report of where the order stands, answering the client's `cl_ord_id`.
    fn report(&self, cl_ord_id: &CompactString, kind: ReportKind) -> ClientReport {
        ClientReport {
            kind,
            order_id: Some(self.id),
            cl_ord_id: cl_ord_id.clone(),
            orig_cl_ord_id: None,
            request: self.request.clone(),
            status: self.status,
            reject: None,
            last_qty: Decimal::ZERO,
            last_px: Decimal::ZERO,
            cum_qty: self.cum_qty,
            leaves_qty: self.leaves_qty(),
            avg_px: self.avg_px,
            text: None,
        }
    }

    fn leaves_qty(&self) -> Decimal {
        if self.status.is_done() { Decimal::ZERO } else { self.request.qty - self.cum_qty }
    }
}

/// A report the book has already moved past, such as a venue resend after a reconnect: applying it
/// would roll the order back. Done is final (busts and corrections come later). Fills are known by
/// LastQty, not status, since a fill on an order pending cancel says PendingCancel.
fn stale(order: &Order, report: &VenueReport) -> bool {
    order.status.is_done()
        || report.cum_qty < order.cum_qty
        || (report.last_qty > Decimal::ZERO && report.cum_qty == order.cum_qty)
}

fn reject(client: ClientId, request: OrderRequest, reason: RejectReason, text: CompactString) -> Output {
    let report = ClientReport {
        kind: ReportKind::Event,
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

    #[test]
    fn a_reused_cl_ord_id_is_rejected_as_a_duplicate() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = new_order(&mut book, "C1");
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, Some(RejectReason::Duplicate));
    }

    #[test]
    fn a_redelivered_order_gets_its_state_not_a_second_order() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true });
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.order_id, report.status, report.reject), (Some(OrderId(1)), Status::PendingNew, None));
        assert_eq!(report.kind, ReportKind::Status);
        assert_eq!(report.leaves_qty, Decimal::from(100));
        assert_eq!(report.cl_ord_id, "C1");
    }

    #[test]
    fn a_redelivered_order_we_never_saw_is_booked() {
        let mut book = book();
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true });
        assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
    }

    #[test]
    fn a_redelivery_for_a_different_order_is_still_a_duplicate() {
        let mut book = book();
        new_order(&mut book, "C1");
        let mut other = request("C1");
        other.side = Side::Sell;
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: other, redelivered: true });
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, Some(RejectReason::Duplicate));
    }

    #[test]
    fn the_same_cl_ord_id_from_another_client_is_a_new_order() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out =
            run(&mut book, Input::NewOrder { client: "FUNDB".into(), request: request("C1"), redelivered: false });
        assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
    }

    #[test]
    fn a_client_just_under_its_cl_ord_id_bound_is_accepted() {
        let mut book = book();
        book.used.insert("FUNDA".into(), MAX_CL_ORD_IDS_PER_CLIENT - 1);
        let out = new_order(&mut book, "C1");
        assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
    }

    #[test]
    fn a_client_at_its_cl_ord_id_bound_is_rejected() {
        let mut book = book();
        book.used.insert("FUNDA".into(), MAX_CL_ORD_IDS_PER_CLIENT);
        let out = new_order(&mut book, "C1");
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, Some(RejectReason::TooManyOrders));
    }

    fn venue_report(downstream: &str, status: Status, last_qty: i64, cum_qty: i64) -> Input {
        Input::VenueReport {
            venue: "NASDAQ".into(),
            report: VenueReport {
                downstream: downstream.into(),
                status,
                last_qty: Decimal::from(last_qty),
                last_px: Decimal::from(10),
                cum_qty: Decimal::from(cum_qty),
                avg_px: Decimal::from(10),
                text: None,
            },
        }
    }

    #[test]
    fn a_fill_goes_back_under_the_clients_cl_ord_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        let [Output::Report { client, report }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((client.as_str(), report.cl_ord_id.as_str()), ("FUNDA", "C1"));
        assert_eq!((report.last_qty, report.cum_qty, report.leaves_qty), (40.into(), 40.into(), 60.into()));
        assert_eq!(report.kind, ReportKind::Event);
        assert_eq!((report.last_px, report.avg_px, &report.orig_cl_ord_id), (10.into(), 10.into(), &None));
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((order.status, order.cum_qty, order.avg_px), (Status::PartiallyFilled, 40.into(), 10.into()));
    }

    #[test]
    fn a_resent_ack_after_a_fill_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        assert!(run(&mut book, venue_report("1", Status::New, 0, 0)).is_empty());
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((order.status, order.cum_qty), (Status::PartiallyFilled, 40.into()));
    }

    #[test]
    fn a_done_order_takes_no_more_reports() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::Filled, 100, 100));
        assert!(run(&mut book, venue_report("1", Status::New, 0, 0)).is_empty());
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Filled);
    }

    #[test]
    fn an_older_fill_after_a_newer_one_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        run(&mut book, venue_report("1", Status::Filled, 60, 100));
        assert!(run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40)).is_empty());
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((order.status, order.cum_qty), (Status::Filled, 100.into()));
    }

    #[test]
    fn a_report_without_a_fill_at_the_same_cum_qty_is_forwarded() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        let out = run(&mut book, venue_report("1", Status::PendingCancel, 0, 40));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.status, report.cum_qty), (Status::PendingCancel, 40.into()));
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingCancel);
    }

    #[test]
    fn an_overfill_is_refused() {
        let mut book = book();
        new_order(&mut book, "C1");
        let before = book.order(OrderId(1)).unwrap().clone();
        assert!(run(&mut book, venue_report("1", Status::Filled, 150, 150)).is_empty());
        assert_eq!(book.order(OrderId(1)).unwrap(), &before);
    }

    #[test]
    fn a_resent_fill_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        assert!(run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40)).is_empty());
        assert_eq!(book.order(OrderId(1)).unwrap().cum_qty, Decimal::from(40));
    }

    #[test]
    fn a_report_for_an_unknown_order_is_not_forwarded() {
        let mut book = book();
        assert!(run(&mut book, venue_report("99", Status::New, 0, 0)).is_empty());
    }

    #[test]
    fn a_report_from_another_venue_is_not_forwarded() {
        let mut book = book();
        new_order(&mut book, "C1");
        let Input::VenueReport { report, .. } = venue_report("1", Status::New, 0, 0) else { unreachable!() };
        assert!(run(&mut book, Input::VenueReport { venue: "LSE".into(), report }).is_empty());
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingNew);
    }
}
