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

/// ClOrdIDs past [`MAX_CL_ORD_IDS_PER_CLIENT`] kept for cancels and replaces, so a client refused
/// new orders can still take its working orders off the market. It also keeps each order's
/// `amendments` far below `u32::MAX`.
pub const MAX_AMEND_RESERVE: usize = 10_000;

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

/// ExecType(150): what a report says happened. The FIX side maps it per version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecType {
    PendingNew,
    New,
    Trade,
    PendingCancel,
    Canceled,
    PendingReplace,
    Replaced,
    Rejected,
    Expired,
    Restated,
    DoneForDay,
    /// Nothing new, the order's state: ExecType=I (4.3+) or ExecTransType=3 (4.2).
    Status,
}

/// Why an order was rejected; the FIX side maps it to OrdRejReason(103).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Duplicate,
    /// No routing rule matched.
    NoRoute,
    /// A routing rule rejects it.
    RouteRejected,
    TooManyOrders,
    /// The venue's OrdRejReason, passed through.
    Venue(u32),
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
    /// A replace sent to the venue and not yet answered; `request` changes only when it's accepted.
    pub pending: Option<Pending>,
    /// Client reports built for the order so far, numbering the next ExecID.
    pub exec_seq: u64,
    /// The venue's OrderID(37): from the first report the book took that had one, then from the
    /// report accepting each replace. "NONE" and empty ones don't count.
    pub venue_order_id: Option<CompactString>,
}

/// A replace awaiting the venue's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub downstream: CompactString,
    pub cl_ord_id: CompactString,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    pub ord_type: OrdType,
    pub time_in_force: Option<TimeInForce>,
}

/// An ExecutionReport from a venue, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueReport {
    /// The downstream ClOrdID it answers.
    pub downstream: CompactString,
    /// Passed to the client as it is; fills are still known by `last_qty`, not `Trade`.
    pub exec_type: ExecType,
    pub status: Status,
    pub last_qty: Decimal,
    pub last_px: Decimal,
    pub cum_qty: Decimal,
    pub avg_px: Decimal,
    /// ExecID(17), for the logs: not a dedupe key, since `stale` goes by CumQty.
    pub exec_id: CompactString,
    pub text: Option<CompactString>,
    /// OrderID(37), the venue's: some venues want it on cancels and replaces.
    pub venue_order_id: Option<CompactString>,
    /// OrdRejReason(103), passed to the client when the venue rejects the order.
    pub ord_rej_reason: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// `redelivered`: Turbojet's `maybe_redelivered()`, PossResend(97) or PossDupFlag(43).
    NewOrder {
        client: ClientId,
        request: OrderRequest,
        redelivered: bool,
    },
    /// `side` and `symbol` must be the order's, as FIX requires them on the request.
    Cancel {
        client: ClientId,
        cl_ord_id: CompactString,
        orig_cl_ord_id: CompactString,
        side: Side,
        symbol: CompactString,
    },
    /// As `Cancel`; `ord_type` and `time_in_force` are `None` to keep the order's. A replace to
    /// Market drops the price.
    Replace {
        client: ClientId,
        cl_ord_id: CompactString,
        orig_cl_ord_id: CompactString,
        side: Side,
        symbol: CompactString,
        qty: Decimal,
        price: Option<Decimal>,
        ord_type: Option<OrdType>,
        time_in_force: Option<TimeInForce>,
    },
    VenueReport {
        venue: VenueId,
        report: VenueReport,
    },
    VenueCancelReject {
        venue: VenueId,
        downstream: CompactString,
        /// OrdStatus(39), required on OrderCancelReject.
        status: Status,
        text: Option<CompactString>,
        /// CxlRejReason(102).
        reason: Option<u32>,
        /// OrderID(37), the venue's.
        venue_order_id: Option<CompactString>,
    },
}

/// An ExecutionReport for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientReport {
    /// The venue's, except for the book's own rejects (`Rejected`) and answers to redeliveries
    /// (`Status`).
    pub exec_type: ExecType,
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
    /// ExecID(17): `<OrderID>.<n>` for a booked order, `R<n>` for a reject before booking.
    pub exec_id: CompactString,
    pub text: Option<CompactString>,
}

/// What an OrderCancelReject answers: ResponseTo(434).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelKind {
    Cancel,
    Replace,
}

/// CxlRejReason(102): 0, 1, 6 and 99, or the venue's; the FIX side maps it per version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRejectReason {
    TooLate,
    UnknownOrder,
    Duplicate,
    Other,
    /// The venue's CxlRejReason, passed through.
    Venue(u32),
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
        venue_order_id: Option<CompactString>,
    },
    /// `request` carries the new quantity, price, OrdType and TimeInForce.
    Replace {
        venue: VenueId,
        downstream: CompactString,
        orig_downstream: CompactString,
        request: OrderRequest,
        venue_order_id: Option<CompactString>,
    },
}

/// A request we sent a venue, by its downstream ClOrdID: what the venue's answers map back to.
#[derive(Debug, Clone)]
struct Sent {
    order: OrderId,
    /// The client's ClOrdID for the request, and its OrigClOrdID for a cancel or replace.
    cl_ord_id: CompactString,
    orig_cl_ord_id: Option<CompactString>,
    /// `None` for the original order.
    kind: Option<CancelKind>,
    /// The venue has rejected or accepted it (a cancel's Canceled, a replace's acceptance): a
    /// later reject is a resend.
    answered: bool,
}

#[derive(Debug)]
pub struct Book {
    router: Router,
    orders: HashMap<OrderId, Order>,
    /// (client, ClOrdID) -> order, for every ClOrdID each client used today.
    index: HashMap<(ClientId, CompactString), OrderId>,
    /// ClOrdIDs in `index` per client, against [`MAX_CL_ORD_IDS_PER_CLIENT`].
    used: HashMap<ClientId, usize>,
    /// Downstream ClOrdID -> what we sent under it.
    downstream: HashMap<CompactString, Sent>,
    next_id: u64,
    /// Rejects before booking so far, numbering their ExecIDs.
    rejects: u64,
    /// [`MAX_CL_ORD_IDS_PER_CLIENT`] and [`MAX_AMEND_RESERVE`]; fields so tests can reach them
    /// with real orders rather than a million.
    max_cl_ord_ids: usize,
    amend_reserve: usize,
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
            rejects: 0,
            max_cl_ord_ids: MAX_CL_ORD_IDS_PER_CLIENT,
            amend_reserve: MAX_AMEND_RESERVE,
        }
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    pub fn handle(&mut self, input: Input, out: &mut Vec<Output>) {
        match input {
            Input::NewOrder { client, request, redelivered } => self.new_order(client, request, redelivered, out),
            Input::VenueReport { venue, report } => self.venue_report(&venue, report, out),
            Input::Cancel { client, cl_ord_id, orig_cl_ord_id, side, symbol } => {
                self.cancel(Amend { client, cl_ord_id, orig_cl_ord_id, side, symbol, kind: CancelKind::Cancel }, out)
            }
            Input::Replace { client, cl_ord_id, orig_cl_ord_id, side, symbol, qty, price, ord_type, time_in_force } => {
                let amend = Amend { client, cl_ord_id, orig_cl_ord_id, side, symbol, kind: CancelKind::Replace };
                self.replace(amend, qty, price, ord_type, time_in_force, out)
            }
            Input::VenueCancelReject { venue, downstream, status, text, reason, venue_order_id } => {
                let refused = Refused { status: Some(status), text, venue_order_id, reason };
                self.venue_cancel_reject(&venue, &downstream, refused, out)
            }
        }
        #[cfg(debug_assertions)]
        self.check();
    }

    /// The book's invariants, after every input. Walks the whole book, so debug builds only.
    #[cfg(debug_assertions)]
    fn check(&self) {
        for (id, order) in &self.orders {
            let sent = self.downstream.get(&order.downstream).expect("an order's downstream is recorded");
            assert_eq!(sent.order, *id);
            assert_ne!(sent.kind, Some(CancelKind::Cancel), "an order's downstream is never a cancel's");
            if let Some(pending) = &order.pending {
                assert!(!order.status.is_done(), "done order {id:?} has a replace pending");
                let sent = self.downstream.get(&pending.downstream).expect("a pending replace is recorded");
                assert_eq!(sent.order, *id);
                assert_eq!(sent.kind, Some(CancelKind::Replace));
            }
        }
        for (client, &used) in &self.used {
            let indexed = self.index.keys().filter(|(owner, _)| owner == client).count();
            assert_eq!(used, indexed, "{client}'s ClOrdID count");
        }
    }

    fn new_order(&mut self, client: ClientId, request: OrderRequest, redelivered: bool, out: &mut Vec<Output>) {
        if let Some(&id) = self.index.get(&(client.clone(), request.cl_ord_id.clone())) {
            let order = self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders");
            // A resend of an order we booked: answer with where it stands. Side, symbol and account,
            // not the whole request, since a replace may have changed quantity and price since.
            let resent = redelivered
                && order.request.side == request.side
                && order.request.symbol == request.symbol
                && order.request.account == request.account;
            let output = if resent {
                // Reports carry the order's latest ClOrdID; after a replace, OrigClOrdID names the
                // one the client asked about.
                let mut report = order.report(&order.request.cl_ord_id.clone(), ExecType::Status);
                if report.cl_ord_id != request.cl_ord_id {
                    report.orig_cl_ord_id = Some(request.cl_ord_id);
                }
                Output::Report { client, report }
            } else {
                self.reject(client, request, RejectReason::Duplicate, "duplicate ClOrdID".into())
            };
            return out.push(output);
        }
        if self.used.get(&client).is_some_and(|&used| used >= self.max_cl_ord_ids) {
            return out.push(self.reject(client, request, RejectReason::TooManyOrders, "too many orders today".into()));
        }
        let venue = match self.router.route(&client, &request) {
            Some(Target::Venue(venue)) => venue,
            Some(Target::Reject(text)) => {
                return out.push(self.reject(client, request, RejectReason::RouteRejected, text));
            }
            None => {
                let text = "no route for this order".into();
                return out.push(self.reject(client, request, RejectReason::NoRoute, text));
            }
        };
        let id = OrderId(self.next_id);
        self.next_id += 1;
        let downstream = format_compact!("{}", id.0);
        self.remember(&client, request.cl_ord_id.clone(), id);
        let sent =
            Sent { order: id, cl_ord_id: request.cl_ord_id.clone(), orig_cl_ord_id: None, kind: None, answered: false };
        let previous = self.downstream.insert(downstream.clone(), sent);
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
            pending: None,
            exec_seq: 0,
            venue_order_id: None,
        };
        let previous = self.orders.insert(id, order);
        debug_assert!(previous.is_none(), "order {id:?} booked twice");
    }

    fn venue_report(&mut self, venue: &VenueId, report: VenueReport, out: &mut Vec<Output>) {
        let Some(sent) = self.downstream.get(&report.downstream).cloned() else {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "execution report for an unknown order");
        };
        // Rejected under a cancel's or replace's ClOrdID is about that request, not the order.
        // Some venues refuse an amendment with ExecType=8 and the order's live OrdStatus: that
        // status stands, as an OrderCancelReject's would.
        let rejected = report.status == Status::Rejected || report.exec_type == ExecType::Rejected;
        if rejected && sent.kind.is_some() {
            let status = (report.status != Status::Rejected).then_some(report.status);
            // An ExecutionReport has no CxlRejReason: the refusal's reason is Other.
            let refused = Refused { status, text: report.text, venue_order_id: report.venue_order_id, reason: None };
            return self.refuse_request(venue, &report.downstream, sent, refused, out);
        }
        let Sent { order: id, cl_ord_id, orig_cl_ord_id, kind, .. } = sent;
        let order = self.orders.get_mut(&id).expect("downstream ClOrdIDs point at booked orders");
        if order.venue != *venue {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "execution report for another venue's order");
        }
        if stale(order, &report) {
            return tracing::debug!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "stale execution report dropped");
        }
        let pending_here = order.pending.as_ref().is_some_and(|pending| pending.downstream == report.downstream);
        // Any answer under the replace's ClOrdID but these means the venue took it.
        let accepted = pending_here && !rejected && report.status != Status::PendingReplace;
        // A cancel's reports and a pending replace's (up to the one accepting it) answer that
        // request; any other report is about the order, under the client's latest ClOrdID.
        let answers_request = match kind {
            Some(CancelKind::Cancel) => true,
            Some(CancelKind::Replace) => pending_here,
            None => false,
        };
        let qty = match &order.pending {
            Some(pending) if accepted => pending.qty,
            _ => order.request.qty,
        };
        if report.cum_qty > qty {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, cum_qty = %report.cum_qty, "overfill");
        }
        order.record_venue_order_id(report.venue_order_id, accepted);
        if accepted {
            let pending = order.pending.take().expect("checked above");
            order.request.cl_ord_id = pending.cl_ord_id;
            order.request.qty = pending.qty;
            order.request.price = pending.price;
            order.request.ord_type = pending.ord_type;
            order.request.time_in_force = pending.time_in_force;
            order.downstream = pending.downstream;
        }
        if accepted || (kind == Some(CancelKind::Cancel) && report.status == Status::Canceled) {
            self.downstream.get_mut(&report.downstream).expect("looked up above").answered = true;
        }
        order.cum_qty = report.cum_qty;
        order.avg_px = report.avg_px;
        order.set_status(report.status);
        let mut client_report = if answers_request {
            let mut client_report = order.report(&cl_ord_id, report.exec_type);
            client_report.orig_cl_ord_id = orig_cl_ord_id;
            client_report
        } else {
            order.report(&order.request.cl_ord_id.clone(), report.exec_type)
        };
        client_report.last_qty = report.last_qty;
        client_report.last_px = report.last_px;
        client_report.text = report.text;
        if rejected {
            client_report.reject = report.ord_rej_reason.map(RejectReason::Venue);
        }
        out.push(Output::Report { client: order.client.clone(), report: client_report });
    }

    /// A cancel goes to the venue even while PendingNew, under the next numbered downstream ClOrdID.
    fn cancel(&mut self, amend: Amend, out: &mut Vec<Output>) {
        let Some((id, amend)) = self.amendable(amend, |_| None, out) else { return };
        let (downstream, order) = self.number(id, amend);
        out.push(Output::Cancel {
            venue: order.venue.clone(),
            downstream,
            orig_downstream: order.downstream.clone(),
            request: order.request.clone(),
            venue_order_id: order.venue_order_id.clone(),
        });
    }

    /// A replace goes to the venue like a cancel, one at a time; the order keeps its terms until
    /// the venue accepts it (see `venue_report`).
    fn replace(
        &mut self,
        amend: Amend,
        qty: Decimal,
        price: Option<Decimal>,
        ord_type: Option<OrdType>,
        time_in_force: Option<TimeInForce>,
        out: &mut Vec<Output>,
    ) {
        let refuse = |order: &Order| {
            if order.pending.is_some() {
                Some("replace pending")
            } else if qty < order.cum_qty {
                Some("quantity below filled quantity")
            } else {
                None
            }
        };
        let Some((id, amend)) = self.amendable(amend, refuse, out) else { return };
        let cl_ord_id = amend.cl_ord_id.clone();
        let (downstream, order) = self.number(id, amend);
        let ord_type = ord_type.unwrap_or(order.request.ord_type);
        let time_in_force = time_in_force.or(order.request.time_in_force);
        // A market order has no price; the edge refuses a Market replace that gives one.
        let price = if ord_type == OrdType::Market { None } else { price };
        out.push(Output::Replace {
            venue: order.venue.clone(),
            downstream: downstream.clone(),
            orig_downstream: order.downstream.clone(),
            request: OrderRequest { qty, price, ord_type, time_in_force, ..order.request.clone() },
            venue_order_id: order.venue_order_id.clone(),
        });
        order.pending = Some(Pending { downstream, cl_ord_id, qty, price, ord_type, time_in_force });
    }

    /// The live order a cancel or replace names, or `None` with an OrderCancelReject refusing it
    /// pushed. `refuse` adds the request's own checks, as reason Other, after the shared ones,
    /// the last of which is that the request names the order's side and symbol.
    fn amendable(
        &self,
        amend: Amend,
        refuse: impl Fn(&Order) -> Option<&'static str>,
        out: &mut Vec<Output>,
    ) -> Option<(OrderId, Amend)> {
        let id = self.index.get(&(amend.client.clone(), amend.orig_cl_ord_id.clone()));
        let order = id.map(|id| self.orders.get(id).expect("indexed ClOrdIDs point at booked orders"));
        // Before `remember`, which asserts the ClOrdID is new.
        let duplicate = self.index.contains_key(&(amend.client.clone(), amend.cl_ord_id.clone()));
        let refusal = match order {
            _ if duplicate => Some((CancelRejectReason::Duplicate, "duplicate ClOrdID")),
            None => Some((CancelRejectReason::UnknownOrder, "unknown order")),
            Some(order) if order.status.is_done() => Some((CancelRejectReason::TooLate, "order is done")),
            Some(_)
                if self
                    .used
                    .get(&amend.client)
                    .is_some_and(|&used| used >= self.max_cl_ord_ids + self.amend_reserve) =>
            {
                Some((CancelRejectReason::Other, "too many requests today"))
            }
            Some(order) if order.request.side != amend.side || order.request.symbol != amend.symbol => {
                Some((CancelRejectReason::Other, "side or symbol does not match"))
            }
            Some(order) => refuse(order).map(|text| (CancelRejectReason::Other, text)),
        };
        if let Some((reason, text)) = refusal {
            out.push(amend.refuse(order, reason, text.into()));
            return None;
        }
        Some((order.expect("an unknown order is refused").id, amend))
    }

    /// Sends `amend` under the order's next numbered downstream ClOrdID, remembering what it was.
    fn number(&mut self, id: OrderId, amend: Amend) -> (CompactString, &mut Order) {
        let order = self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders");
        order.amendments += 1;
        let downstream = format_compact!("{}-{}", id.0, order.amendments);
        let sent = Sent {
            order: id,
            cl_ord_id: amend.cl_ord_id.clone(),
            orig_cl_ord_id: Some(amend.orig_cl_ord_id),
            kind: Some(amend.kind),
            answered: false,
        };
        let previous = self.downstream.insert(downstream.clone(), sent);
        debug_assert!(previous.is_none(), "downstream ClOrdID {downstream} reused");
        self.remember(&amend.client, amend.cl_ord_id, id);
        (downstream, self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders"))
    }

    /// The venue refused a cancel or replace we sent under `downstream`, with the order's status
    /// as the venue now gives it.
    fn venue_cancel_reject(
        &mut self,
        venue: &VenueId,
        downstream: &CompactString,
        refused: Refused,
        out: &mut Vec<Output>,
    ) {
        let Some(sent) = self.downstream.get(downstream).cloned() else {
            return tracing::warn!(%venue, %downstream, "cancel reject for an unknown request");
        };
        self.refuse_request(venue, downstream, sent, refused, out);
    }

    /// The venue refused the cancel or replace `sent` under `downstream`, by OrderCancelReject or
    /// by a Rejected ExecutionReport: tell the client, under the ClOrdIDs of its own request.
    /// Anything that doesn't name such a request is dropped.
    fn refuse_request(
        &mut self,
        venue: &VenueId,
        downstream: &CompactString,
        sent: Sent,
        refused: Refused,
        out: &mut Vec<Output>,
    ) {
        let Refused { status, text, venue_order_id, reason } = refused;
        let (Some(response_to), Some(orig_cl_ord_id)) = (sent.kind, sent.orig_cl_ord_id) else {
            return tracing::warn!(%venue, %downstream, "request rejected naming an order, not a cancel or replace");
        };
        let order = self.orders.get_mut(&sent.order).expect("downstream ClOrdIDs point at booked orders");
        if order.venue != *venue {
            return tracing::warn!(%venue, %downstream, "request rejected for another venue's order");
        }
        if sent.answered {
            return tracing::debug!(%venue, %downstream, "rejection of an answered request dropped");
        }
        self.downstream.get_mut(downstream).expect("looked up by the caller").answered = true;
        // The venue named its order while refusing the request: as good as any report's.
        order.record_venue_order_id(venue_order_id, false);
        // Only the replace it names: an older one's late reject leaves a newer one pending.
        if order.pending.as_ref().is_some_and(|pending| pending.downstream == *downstream) {
            order.pending = None;
        }
        match status {
            // Done is final, as in `stale`.
            _ if order.status.is_done() => {}
            Some(status) => order.set_status(status),
            // Without the venue's word, the refused request no longer holds the order pending.
            None if matches!(order.status, Status::PendingCancel | Status::PendingReplace) => {
                order.set_status(if order.cum_qty > Decimal::ZERO { Status::PartiallyFilled } else { Status::New });
            }
            None => {}
        }
        let amend = Amend {
            client: order.client.clone(),
            cl_ord_id: sent.cl_ord_id,
            orig_cl_ord_id,
            side: order.request.side,
            symbol: order.request.symbol.clone(),
            kind: response_to,
        };
        let text = text.unwrap_or_else(|| "rejected by the venue".into());
        let reason = reason.map_or(CancelRejectReason::Other, CancelRejectReason::Venue);
        out.push(amend.refuse(Some(&*order), reason, text));
    }

    /// A reject for an order we didn't book, under the book's next `R` ExecID.
    fn reject(&mut self, client: ClientId, request: OrderRequest, reason: RejectReason, text: CompactString) -> Output {
        // Saturating, as in `Order::report`; a u64 of rejects can't be reached.
        self.rejects = self.rejects.saturating_add(1);
        let report = ClientReport {
            exec_type: ExecType::Rejected,
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
            exec_id: format_compact!("R{}", self.rejects),
            text: Some(text),
        };
        Output::Report { client, report }
    }

    fn remember(&mut self, client: &ClientId, cl_ord_id: CompactString, id: OrderId) {
        let previous = self.index.insert((client.clone(), cl_ord_id), id);
        debug_assert!(previous.is_none(), "ClOrdID reused by {client}");
        *self.used.entry(client.clone()).or_default() += 1;
    }
}

impl Order {
    /// A report of where the order stands, answering the client's `cl_ord_id`, under the order's
    /// next ExecID.
    fn report(&mut self, cl_ord_id: &CompactString, exec_type: ExecType) -> ClientReport {
        // Status answers to redeliveries and pass-through venue reports aren't bounded by the
        // ClOrdID limits, so a peer drives this count; at u64 it can't reach the top. Saturating
        // anyway, so no input can panic the book.
        self.exec_seq = self.exec_seq.saturating_add(1);
        ClientReport {
            exec_type,
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
            exec_id: format_compact!("{}.{}", self.id.0, self.exec_seq),
            text: None,
        }
    }

    /// A done order has nothing pending: a replace it still awaited can't happen now. Set after
    /// `cum_qty`: a venue's New on an order with fills (a cancel reject's OrdStatus, say) is
    /// PartiallyFilled to the client.
    fn set_status(&mut self, status: Status) {
        self.status = match status {
            Status::New | Status::PendingNew if self.cum_qty > Decimal::ZERO => Status::PartiallyFilled,
            status => status,
        };
        if self.status.is_done() {
            self.pending = None;
        }
    }

    /// Keeps the first OrderID the venue gives, or the one from the report accepting a replace
    /// (`replaced`), since some venues give the replacing order a new one. Any other different
    /// one is logged and ignored. "NONE" and empty are FIX's placeholders for no order, as when a
    /// venue refuses a request for an order it doesn't know yet.
    fn record_venue_order_id(&mut self, venue_order_id: Option<CompactString>, replaced: bool) {
        let venue_order_id = venue_order_id.filter(|id| !id.is_empty() && id != "NONE");
        match (&self.venue_order_id, venue_order_id) {
            (_, None) => {}
            (None, Some(new)) => self.venue_order_id = Some(new),
            (Some(_), Some(new)) if replaced => self.venue_order_id = Some(new),
            (Some(kept), Some(new)) if *kept != new => {
                tracing::warn!(order = self.id.0, %kept, %new, "venue changed its OrderID; keeping the first");
            }
            (Some(_), Some(_)) => {}
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

/// A venue's refusal of a cancel or replace. `status` is the order's status as the venue gives
/// it, if it does.
struct Refused {
    status: Option<Status>,
    text: Option<CompactString>,
    venue_order_id: Option<CompactString>,
    /// The venue's CxlRejReason; `None` reports Other.
    reason: Option<u32>,
}

/// A client's cancel or replace request.
struct Amend {
    client: ClientId,
    cl_ord_id: CompactString,
    orig_cl_ord_id: CompactString,
    /// As the request names them; the order's must match.
    side: Side,
    symbol: CompactString,
    kind: CancelKind,
}

impl Amend {
    /// An OrderCancelReject refusing it, naming the order when there is one.
    fn refuse(self, order: Option<&Order>, reason: CancelRejectReason, text: CompactString) -> Output {
        Output::CancelReject {
            client: self.client,
            order_id: order.map(|order| order.id),
            cl_ord_id: self.cl_ord_id,
            orig_cl_ord_id: self.orig_cl_ord_id,
            response_to: self.kind,
            status: order.map_or(Status::Rejected, |order| order.status),
            reason,
            text,
        }
    }
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
        assert_eq!(report.exec_type, ExecType::Rejected);
        assert!(book.order(OrderId(1)).is_none());
    }

    #[test]
    fn a_reused_cl_ord_id_is_rejected_as_a_duplicate() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = new_order(&mut book, "C1");
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.reject, report.exec_type), (Some(RejectReason::Duplicate), ExecType::Rejected));
    }

    #[test]
    fn a_redelivered_order_gets_its_state_not_a_second_order() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true });
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.order_id, report.status, report.reject), (Some(OrderId(1)), Status::PendingNew, None));
        assert_eq!(report.exec_type, ExecType::Status);
        assert_eq!(report.leaves_qty, Decimal::from(100));
        assert_eq!(report.cl_ord_id, "C1");
        assert_eq!(report.orig_cl_ord_id, None);
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
        book.max_cl_ord_ids = 2;
        new_order(&mut book, "C1");
        let out = new_order(&mut book, "C2");
        assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
    }

    #[test]
    fn a_client_at_its_cl_ord_id_bound_is_rejected() {
        let mut book = book();
        book.max_cl_ord_ids = 1;
        new_order(&mut book, "C1");
        let out = new_order(&mut book, "C2");
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.reject, report.exec_type), (Some(RejectReason::TooManyOrders), ExecType::Rejected));
    }

    /// A venue report with the ExecType a venue would give it: Trade for a fill, else by status.
    fn venue_report(downstream: &str, status: Status, last_qty: i64, cum_qty: i64) -> Input {
        let exec_type = match status {
            _ if last_qty > 0 => ExecType::Trade,
            Status::PendingNew => ExecType::PendingNew,
            Status::New => ExecType::New,
            Status::PendingCancel => ExecType::PendingCancel,
            Status::PendingReplace => ExecType::PendingReplace,
            Status::Canceled => ExecType::Canceled,
            Status::Expired => ExecType::Expired,
            Status::Rejected => ExecType::Rejected,
            Status::PartiallyFilled | Status::Filled => ExecType::Status,
        };
        venue_report_as(downstream, exec_type, status, last_qty, cum_qty)
    }

    fn venue_report_as(downstream: &str, exec_type: ExecType, status: Status, last_qty: i64, cum_qty: i64) -> Input {
        Input::VenueReport {
            venue: "NASDAQ".into(),
            report: VenueReport {
                downstream: downstream.into(),
                exec_type,
                status,
                last_qty: Decimal::from(last_qty),
                last_px: Decimal::from(10),
                cum_qty: Decimal::from(cum_qty),
                avg_px: Decimal::from(10),
                exec_id: "V1".into(),
                text: None,
                venue_order_id: None,
                ord_rej_reason: None,
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
        assert_eq!(report.exec_type, ExecType::Trade);
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

    /// A cancel naming the order's side and symbol, as `request` has them.
    fn cancel(book: &mut Book, cl_ord_id: &str, orig: &str) -> Vec<Output> {
        run(book, cancel_input(cl_ord_id, orig))
    }

    fn cancel_input(cl_ord_id: &str, orig: &str) -> Input {
        Input::Cancel {
            client: "FUNDA".into(),
            cl_ord_id: cl_ord_id.into(),
            orig_cl_ord_id: orig.into(),
            side: Side::Buy,
            symbol: "AAPL".into(),
        }
    }

    fn cancel_reject(out: &[Output]) -> (Option<OrderId>, CancelKind, Status, CancelRejectReason) {
        let [Output::CancelReject { order_id, response_to, status, reason, .. }] = out else { panic!("{out:?}") };
        (*order_id, *response_to, *status, *reason)
    }

    #[test]
    fn a_cancel_goes_to_the_venue_with_a_numbered_cl_ord_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        // No venue ack yet: a cancel while PendingNew is still forwarded.
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingNew);
        let out = cancel(&mut book, "C2", "C1");
        assert_eq!(
            out,
            [Output::Cancel {
                venue: "NASDAQ".into(),
                downstream: "1-1".into(),
                orig_downstream: "1".into(),
                request: request("C1"),
                venue_order_id: None,
            }]
        );
    }

    #[test]
    fn the_cancel_ack_names_both_client_cl_ord_ids() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        let out = run(&mut book, venue_report("1-1", Status::Canceled, 0, 0));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
        assert_eq!((report.leaves_qty, report.exec_type), (Decimal::ZERO, ExecType::Canceled));
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Canceled);
    }

    #[test]
    fn cancelling_an_unknown_order_is_refused() {
        let mut book = book();
        let out = cancel(&mut book, "C2", "C1");
        assert_eq!(cancel_reject(&out), (None, CancelKind::Cancel, Status::Rejected, CancelRejectReason::UnknownOrder));
    }

    #[test]
    fn cancelling_a_filled_order_is_refused() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::Filled, 100, 100));
        let out = cancel(&mut book, "C2", "C1");
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Cancel, Status::Filled, CancelRejectReason::TooLate)
        );
    }

    #[test]
    fn a_cancel_reusing_a_cl_ord_id_is_refused() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = cancel(&mut book, "C1", "C1");
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Cancel, Status::PendingNew, CancelRejectReason::Duplicate)
        );
    }

    #[test]
    fn a_venue_cancel_reject_goes_back_to_the_client() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        let [Output::CancelReject { cl_ord_id, orig_cl_ord_id, text, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((cl_ord_id.as_str(), orig_cl_ord_id.as_str()), ("C2", "C1"));
        assert_eq!(text, "rejected by the venue");
        assert_eq!(cancel_reject(&out), (Some(OrderId(1)), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
    }

    fn venue_cancel_reject(venue: &str, downstream: &str, status: Status) -> Input {
        Input::VenueCancelReject {
            venue: venue.into(),
            downstream: downstream.into(),
            status,
            text: None,
            reason: None,
            venue_order_id: None,
        }
    }

    #[test]
    fn a_cancel_can_name_an_earlier_cancel() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        let out = cancel(&mut book, "C3", "C2");
        assert_eq!(
            out,
            [Output::Cancel {
                venue: "NASDAQ".into(),
                downstream: "1-2".into(),
                orig_downstream: "1".into(),
                request: request("C1"),
                venue_order_id: None,
            }]
        );
        let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-2", Status::New));
        let [Output::CancelReject { cl_ord_id, orig_cl_ord_id, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((cl_ord_id.as_str(), orig_cl_ord_id.as_str()), ("C3", "C2"));
    }

    #[test]
    fn a_venue_cancel_reject_for_no_cancel_of_ours_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        assert!(run(&mut book, venue_cancel_reject("LSE", "1-1", Status::New)).is_empty());
        assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-9", Status::New)).is_empty());
        assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1", Status::New)).is_empty());
    }

    #[test]
    fn a_fill_on_the_original_after_a_cancel_reports_the_original_cl_ord_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        let out = run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), &report.orig_cl_ord_id), ("C1", &None));
    }

    #[test]
    fn a_venue_cancel_reject_sets_the_orders_status() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0));
        let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        assert_eq!(cancel_reject(&out).2, Status::New);
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
    }

    #[test]
    fn a_venue_cancel_reject_leaves_a_done_order_done() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        run(&mut book, venue_report("1", Status::Filled, 100, 100));
        let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        assert_eq!(cancel_reject(&out).2, Status::Filled);
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Filled);
    }

    /// A replace naming the order's side and symbol, as `request` has them, keeping its OrdType
    /// and TimeInForce.
    fn replace(book: &mut Book, cl_ord_id: &str, orig: &str, qty: i64, price: i64) -> Vec<Output> {
        run(book, replace_input(cl_ord_id, orig, qty, price))
    }

    fn replace_input(cl_ord_id: &str, orig: &str, qty: i64, price: i64) -> Input {
        Input::Replace {
            client: "FUNDA".into(),
            cl_ord_id: cl_ord_id.into(),
            orig_cl_ord_id: orig.into(),
            side: Side::Buy,
            symbol: "AAPL".into(),
            qty: Decimal::from(qty),
            price: Some(Decimal::from(price)),
            ord_type: None,
            time_in_force: None,
        }
    }

    fn replaced(cl_ord_id: &str, qty: i64, price: i64) -> OrderRequest {
        OrderRequest { qty: Decimal::from(qty), price: Some(Decimal::from(price)), ..request(cl_ord_id) }
    }

    #[test]
    fn a_replace_goes_to_the_venue_with_a_numbered_cl_ord_id_and_the_new_terms() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = replace(&mut book, "C2", "C1", 80, 11);
        assert_eq!(
            out,
            [Output::Replace {
                venue: "NASDAQ".into(),
                downstream: "1-1".into(),
                orig_downstream: "1".into(),
                request: replaced("C1", 80, 11),
                venue_order_id: None,
            }]
        );
        assert_eq!(book.order(OrderId(1)).unwrap().request, request("C1"));
    }

    #[test]
    fn a_pending_replace_report_is_forwarded_without_applying_the_replace() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, venue_report("1-1", Status::PendingReplace, 0, 0));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
        assert_eq!(report.status, Status::PendingReplace);
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((&order.request, order.downstream.as_str()), (&request("C1"), "1"));
        assert!(order.pending.is_some());
    }

    #[test]
    fn the_venues_ack_applies_the_replace() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
        assert_eq!((report.request.qty, report.leaves_qty), (80.into(), 80.into()));
        assert_eq!(report.exec_type, ExecType::Replaced);
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((&order.request, order.downstream.as_str()), (&replaced("C2", 80, 11), "1-1"));
        assert!(order.pending.is_none());
    }

    #[test]
    fn a_cancel_after_a_replace_names_the_replaced_downstream() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, venue_report("1-1", Status::New, 0, 0));
        let out = cancel(&mut book, "C3", "C2");
        assert_eq!(
            out,
            [Output::Cancel {
                venue: "NASDAQ".into(),
                downstream: "1-2".into(),
                orig_downstream: "1-1".into(),
                request: replaced("C2", 80, 11),
                venue_order_id: None,
            }]
        );
    }

    #[test]
    fn a_rejected_replace_leaves_the_order_as_it_was() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Replace, Status::New, CancelRejectReason::Other)
        );
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((&order.request, order.downstream.as_str()), (&request("C1"), "1"));
        assert!(order.pending.is_none());
        // Nothing is pending any more, so another replace goes out.
        assert!(matches!(replace(&mut book, "C3", "C1", 70, 11).as_slice(), [Output::Replace { .. }]));
    }

    #[test]
    fn a_fill_under_the_original_downstream_after_a_replace_reports_the_latest_cl_ord_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, venue_report("1-1", Status::New, 0, 0));
        let out = run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), &report.orig_cl_ord_id), ("C2", &None));
        assert_eq!(report.leaves_qty, Decimal::from(40));
    }

    #[test]
    fn a_fill_under_the_original_downstream_before_the_replace_is_acked_reports_the_original() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), &report.orig_cl_ord_id), ("C1", &None));
    }

    #[test]
    fn a_fill_under_an_accepted_replace_reports_no_orig_cl_ord_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, venue_report("1-1", Status::New, 0, 0));
        let out = run(&mut book, venue_report("1-1", Status::PartiallyFilled, 40, 40));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), &report.orig_cl_ord_id), ("C2", &None));
    }

    #[test]
    fn a_replace_while_another_is_pending_is_refused() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = replace(&mut book, "C3", "C1", 70, 11);
        let [Output::CancelReject { text, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(text, "replace pending");
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Replace, Status::PendingNew, CancelRejectReason::Other)
        );
    }

    #[test]
    fn a_replace_below_the_filled_quantity_is_refused() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        let out = replace(&mut book, "C2", "C1", 30, 10);
        let [Output::CancelReject { text, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(text, "quantity below filled quantity");
        assert!(book.order(OrderId(1)).unwrap().pending.is_none());
    }

    #[test]
    fn replace_refusals_follow_the_cancel_order() {
        let mut book = book();
        let out = replace(&mut book, "C2", "C1", 80, 10);
        assert_eq!(
            cancel_reject(&out),
            (None, CancelKind::Replace, Status::Rejected, CancelRejectReason::UnknownOrder)
        );
        new_order(&mut book, "C1");
        let out = replace(&mut book, "C1", "C1", 80, 10);
        assert_eq!(cancel_reject(&out).3, CancelRejectReason::Duplicate);
        run(&mut book, venue_report("1", Status::Filled, 100, 100));
        let out = replace(&mut book, "C2", "C1", 80, 10);
        assert_eq!(cancel_reject(&out).3, CancelRejectReason::TooLate);
    }

    #[test]
    fn a_fill_up_to_a_reduced_quantity_is_not_an_overfill() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        replace(&mut book, "C2", "C1", 60, 10);
        run(&mut book, venue_report("1-1", Status::PartiallyFilled, 0, 40));
        let out = run(&mut book, venue_report("1-1", Status::Filled, 20, 60));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.status, report.cum_qty, report.leaves_qty), (Status::Filled, 60.into(), 0.into()));
    }

    #[test]
    fn the_fill_that_accepts_a_raised_quantity_is_checked_against_it() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 150, 10);
        // The venue's first word on the replace is a fill beyond the old quantity.
        let out = run(&mut book, venue_report("1-1", Status::Filled, 150, 150));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cum_qty, report.request.qty), (150.into(), 150.into()));
        assert_eq!(report.exec_type, ExecType::Trade);
    }

    #[test]
    fn a_redelivery_after_a_replace_reports_the_current_cl_ord_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, venue_report("1-1", Status::New, 0, 0));
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true });
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
        assert_eq!((report.exec_type, report.reject, report.request.qty), (ExecType::Status, None, 80.into()));
    }

    #[test]
    fn a_rejected_report_for_a_replace_rejects_the_replace_not_the_order() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, venue_report("1-1", Status::Rejected, 0, 0));
        let [Output::CancelReject { cl_ord_id, orig_cl_ord_id, text, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((cl_ord_id.as_str(), orig_cl_ord_id.as_str()), ("C2", "C1"));
        assert_eq!(text, "rejected by the venue");
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Replace, Status::New, CancelRejectReason::Other)
        );
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((order.status, &order.request, order.downstream.as_str()), (Status::New, &request("C1"), "1"));
        assert!(order.pending.is_none());
        // The order is still live, so it can still be cancelled.
        assert!(matches!(cancel(&mut book, "C3", "C1").as_slice(), [Output::Cancel { .. }]));
    }

    #[test]
    fn exec_type_rejected_under_a_replace_rejects_the_replace() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        replace(&mut book, "C2", "C1", 80, 11);
        // Some venues refuse an amendment with ExecType=8 and the order's live OrdStatus.
        let out = run(&mut book, venue_report_as("1-1", ExecType::Rejected, Status::New, 0, 0));
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Replace, Status::New, CancelRejectReason::Other)
        );
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((order.status, &order.request, order.downstream.as_str()), (Status::New, &request("C1"), "1"));
        assert!(order.pending.is_none());
    }

    #[test]
    fn exec_type_rejected_under_a_cancel_rejects_the_cancel() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        cancel(&mut book, "C2", "C1");
        let out = run(&mut book, venue_report_as("1-1", ExecType::Rejected, Status::New, 0, 0));
        assert_eq!(cancel_reject(&out), (Some(OrderId(1)), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
    }

    #[test]
    fn a_restatement_passes_through() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        let out = run(&mut book, venue_report_as("1", ExecType::Restated, Status::New, 0, 0));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.exec_type, report.status), (ExecType::Restated, Status::New));
    }

    #[test]
    fn a_rejected_report_for_a_cancel_rejects_the_cancel_not_the_order() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        cancel(&mut book, "C2", "C1");
        let out = run(&mut book, venue_report("1-1", Status::Rejected, 0, 0));
        assert_eq!(cancel_reject(&out), (Some(OrderId(1)), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
    }

    #[test]
    fn a_rejected_report_for_the_original_order_rejects_the_order() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, venue_report("1", Status::Rejected, 0, 0));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.status, report.exec_type), (Status::Rejected, ExecType::Rejected));
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Rejected);
    }

    #[test]
    fn a_rejected_replace_takes_the_order_out_of_pending_replace() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, venue_report("1-1", Status::PendingReplace, 0, 0));
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingReplace);
        let out = run(&mut book, venue_report("1-1", Status::Rejected, 0, 0));
        assert_eq!(cancel_reject(&out).2, Status::New);
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
    }

    #[test]
    fn a_rejected_cancel_after_a_fill_goes_back_to_partially_filled() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        cancel(&mut book, "C2", "C1");
        run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 40));
        let out = run(&mut book, venue_report("1-1", Status::Rejected, 0, 40));
        assert_eq!(cancel_reject(&out).2, Status::PartiallyFilled);
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PartiallyFilled);
    }

    #[test]
    fn a_done_order_has_no_pending_replace() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        cancel(&mut book, "C3", "C1");
        run(&mut book, venue_report("1-2", Status::Canceled, 0, 0));
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!(order.status, Status::Canceled);
        assert!(order.pending.is_none());
    }

    #[test]
    fn a_cancel_while_a_replace_is_pending_names_the_last_accepted_downstream() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = cancel(&mut book, "C3", "C1");
        assert_eq!(
            out,
            [Output::Cancel {
                venue: "NASDAQ".into(),
                downstream: "1-2".into(),
                orig_downstream: "1".into(),
                request: request("C1"),
                venue_order_id: None,
            }]
        );
    }

    #[test]
    fn a_client_at_its_new_order_bound_can_still_cancel() {
        let mut book = book();
        book.max_cl_ord_ids = 1;
        new_order(&mut book, "C1");
        assert!(matches!(cancel(&mut book, "C2", "C1").as_slice(), [Output::Cancel { .. }]));
    }

    #[test]
    fn a_client_past_its_reserve_cannot_cancel() {
        let mut book = book();
        (book.max_cl_ord_ids, book.amend_reserve) = (1, 1);
        new_order(&mut book, "C1");
        // The reserve's one ClOrdID.
        assert!(matches!(cancel(&mut book, "C2", "C1").as_slice(), [Output::Cancel { .. }]));
        let out = cancel(&mut book, "C3", "C1");
        let [Output::CancelReject { text, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(text, "too many requests today");
        assert_eq!(cancel_reject(&out).3, CancelRejectReason::Other);
        let out = replace(&mut book, "C3", "C1", 80, 10);
        assert_eq!(
            cancel_reject(&out),
            (Some(OrderId(1)), CancelKind::Replace, Status::PendingNew, CancelRejectReason::Other)
        );
    }

    #[test]
    fn a_resent_cancel_reject_after_a_fill_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New)).is_empty());
        let order = book.order(OrderId(1)).unwrap();
        assert_eq!((order.status, order.cum_qty), (Status::PartiallyFilled, 40.into()));
    }

    #[test]
    fn a_resent_rejected_report_for_a_cancel_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        assert_eq!(run(&mut book, venue_report("1-1", Status::Rejected, 0, 0)).len(), 1);
        assert!(run(&mut book, venue_report("1-1", Status::Rejected, 0, 0)).is_empty());
    }

    #[test]
    fn a_reject_for_an_acked_cancel_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        run(&mut book, venue_report("1-1", Status::Canceled, 0, 0));
        assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New)).is_empty());
    }

    #[test]
    fn a_reject_for_an_accepted_replace_is_dropped() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, venue_report("1-1", Status::New, 0, 0));
        assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New)).is_empty());
        assert_eq!(book.order(OrderId(1)).unwrap().request, replaced("C2", 80, 11));
    }

    fn exec_ids(out: &[Output]) -> Vec<&str> {
        out.iter()
            .map(|output| match output {
                Output::Report { report, .. } => report.exec_id.as_str(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn an_orders_exec_ids_count_up_from_its_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        assert_eq!(exec_ids(&run(&mut book, venue_report("1", Status::New, 0, 0))), ["1.1"]);
        assert_eq!(exec_ids(&run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40))), ["1.2"]);
        cancel(&mut book, "C2", "C1");
        assert_eq!(exec_ids(&run(&mut book, venue_report("1-1", Status::Canceled, 0, 40))), ["1.3"]);
    }

    #[test]
    fn a_status_answer_takes_the_next_exec_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true });
        assert_eq!(exec_ids(&out), ["1.2"]);
        assert_eq!(exec_ids(&run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40))), ["1.3"]);
    }

    #[test]
    fn unbooked_rejects_number_their_exec_ids_across_the_book() {
        let mut book = book();
        new_order(&mut book, "C1");
        assert_eq!(exec_ids(&new_order(&mut book, "C1")), ["R1"]);
        let mut req = request("C2");
        req.ex_destination = None;
        let out = run(&mut book, Input::NewOrder { client: "FUNDB".into(), request: req, redelivered: false });
        assert_eq!(exec_ids(&out), ["R2"]);
    }

    #[test]
    fn a_second_orders_exec_ids_start_from_its_own_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::New, 0, 0));
        new_order(&mut book, "C2");
        assert_eq!(exec_ids(&run(&mut book, venue_report("2", Status::New, 0, 0))), ["2.1"]);
    }

    #[test]
    fn a_dropped_venue_report_uses_up_no_exec_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        assert_eq!(exec_ids(&run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40))), ["1.1"]);
        // Stale: a resend of that fill. Overfill: past the quantity.
        assert!(run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40)).is_empty());
        assert!(run(&mut book, venue_report("1", Status::Filled, 110, 150)).is_empty());
        assert_eq!(exec_ids(&run(&mut book, venue_report("1", Status::Filled, 60, 100))), ["1.2"]);
    }

    #[test]
    fn a_status_answer_after_an_acked_replace_takes_the_next_exec_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        assert_eq!(exec_ids(&run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0))), ["1.1"]);
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true });
        assert_eq!(exec_ids(&out), ["1.2"]);
    }

    #[test]
    fn a_venue_status_of_new_on_a_filled_order_is_partially_filled() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        cancel(&mut book, "C2", "C1");
        let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        assert_eq!(cancel_reject(&out).2, Status::PartiallyFilled);
        assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PartiallyFilled);
    }

    /// `input`, a venue report, carrying the venue's OrderID(37).
    fn with_venue_order_id(mut input: Input, venue_order_id: &str) -> Input {
        let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
        report.venue_order_id = Some(venue_order_id.into());
        input
    }

    fn venue_order_id(book: &Book) -> Option<&str> {
        book.order(OrderId(1)).unwrap().venue_order_id.as_deref()
    }

    #[test]
    fn the_venues_order_id_is_kept_from_its_ack() {
        let mut book = book();
        new_order(&mut book, "C1");
        assert_eq!(venue_order_id(&book), None);
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        assert_eq!(venue_order_id(&book), Some("V-1"));
    }

    #[test]
    fn a_later_different_venue_order_id_is_ignored() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        let out = run(&mut book, with_venue_order_id(venue_report("1", Status::PartiallyFilled, 40, 40), "V-2"));
        assert_eq!(out.len(), 1, "the report itself still goes through");
        assert_eq!(venue_order_id(&book), Some("V-1"));
    }

    #[test]
    fn a_report_without_one_keeps_the_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
        assert_eq!(venue_order_id(&book), Some("V-1"));
    }

    #[test]
    fn cancels_and_replaces_carry_the_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        let out = replace(&mut book, "C2", "C1", 80, 11);
        let [Output::Replace { venue_order_id, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(venue_order_id.as_deref(), Some("V-1"));
        let out = cancel(&mut book, "C3", "C1");
        let [Output::Cancel { venue_order_id, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(venue_order_id.as_deref(), Some("V-1"));
    }

    #[test]
    fn cancels_and_replaces_before_an_ack_carry_no_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = replace(&mut book, "C2", "C1", 80, 11);
        let [Output::Replace { venue_order_id, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(*venue_order_id, None);
        let out = cancel(&mut book, "C3", "C1");
        let [Output::Cancel { venue_order_id, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(*venue_order_id, None);
    }

    #[test]
    fn a_dropped_report_records_no_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        let Input::VenueReport { report, .. } = with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1") else {
            unreachable!()
        };
        assert!(run(&mut book, Input::VenueReport { venue: "LSE".into(), report }).is_empty());
        // Overfill.
        assert!(run(&mut book, with_venue_order_id(venue_report("1", Status::Filled, 150, 150), "V-2")).is_empty());
        assert_eq!(venue_order_id(&book), None);
        run(&mut book, with_venue_order_id(venue_report("1", Status::PartiallyFilled, 40, 40), "V-3"));
        // Stale: a resend of that fill, under another ID.
        assert!(
            run(&mut book, with_venue_order_id(venue_report("1", Status::PartiallyFilled, 40, 40), "V-4")).is_empty()
        );
        assert_eq!(venue_order_id(&book), Some("V-3"));
    }

    #[test]
    fn a_rejected_amendment_records_the_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, with_venue_order_id(venue_report("1-1", Status::Rejected, 0, 0), "V-1"));
        assert_eq!(cancel_reject(&out).1, CancelKind::Replace);
        assert_eq!(venue_order_id(&book), Some("V-1"));
    }

    #[test]
    fn none_and_empty_venue_order_ids_count_as_absent() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        // FIX's placeholder from a venue refusing a request for an order it doesn't know yet.
        let rejected = venue_report_as("1-1", ExecType::Rejected, Status::Rejected, 0, 0);
        let out = run(&mut book, with_venue_order_id(rejected, "NONE"));
        assert_eq!(cancel_reject(&out).1, CancelKind::Replace);
        assert_eq!(venue_order_id(&book), None);
        run(&mut book, with_venue_order_id(venue_report("1", Status::PendingNew, 0, 0), ""));
        assert_eq!(venue_order_id(&book), None);
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        assert_eq!(venue_order_id(&book), Some("V-1"));
    }

    #[test]
    fn an_accepted_replace_takes_the_venues_new_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        replace(&mut book, "C2", "C1", 80, 11);
        run(&mut book, with_venue_order_id(venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0), "V-2"));
        assert_eq!(venue_order_id(&book), Some("V-2"));
        // A late report from before the replace doesn't roll it back.
        run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1"));
        assert_eq!(venue_order_id(&book), Some("V-2"));
        let out = cancel(&mut book, "C3", "C2");
        let [Output::Cancel { venue_order_id, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(venue_order_id.as_deref(), Some("V-2"));
    }

    #[test]
    fn a_refusal_of_an_answered_request_records_no_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        assert_eq!(run(&mut book, venue_report("1-1", Status::Rejected, 0, 0)).len(), 1);
        let resent = with_venue_order_id(venue_report("1-1", Status::Rejected, 0, 0), "V-1");
        assert!(run(&mut book, resent).is_empty());
        assert_eq!(venue_order_id(&book), None);
    }

    #[test]
    fn a_venue_reject_passes_its_reason_to_the_client() {
        let mut book = book();
        new_order(&mut book, "C1");
        let mut input = venue_report("1", Status::Rejected, 0, 0);
        let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
        report.ord_rej_reason = Some(3);
        let out = run(&mut book, input);
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, Some(RejectReason::Venue(3)));
    }

    #[test]
    fn a_venue_reject_without_a_reason_gives_none() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, venue_report("1", Status::Rejected, 0, 0));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, None);
    }

    #[test]
    fn a_venue_cancel_reject_passes_its_reason_to_the_client() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        let mut input = venue_cancel_reject("NASDAQ", "1-1", Status::New);
        let Input::VenueCancelReject { reason, .. } = &mut input else { unreachable!() };
        *reason = Some(1);
        assert_eq!(cancel_reject(&run(&mut book, input)).3, CancelRejectReason::Venue(1));
    }

    #[test]
    fn a_venue_cancel_reject_records_the_venue_order_id() {
        let mut book = book();
        new_order(&mut book, "C1");
        cancel(&mut book, "C2", "C1");
        cancel(&mut book, "C3", "C1");
        let with_id = |downstream: &str, id: &str| {
            let mut input = venue_cancel_reject("NASDAQ", downstream, Status::New);
            let Input::VenueCancelReject { venue_order_id, .. } = &mut input else { unreachable!() };
            *venue_order_id = Some(id.into());
            input
        };
        run(&mut book, with_id("1-1", "NONE"));
        assert_eq!(venue_order_id(&book), None);
        run(&mut book, with_id("1-2", "V-1"));
        assert_eq!(venue_order_id(&book), Some("V-1"));
    }

    #[test]
    fn a_rule_reject_says_route_rejected_and_no_match_says_no_route() {
        let mut book = Book::new(Router::new(vec![Rule {
            client: None,
            ex_destination: Some("XNAS".into()),
            symbol: None,
            target: Target::Reject("venue closed".into()),
        }]));
        let out = new_order(&mut book, "C1");
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.reject, report.text.as_deref()), (Some(RejectReason::RouteRejected), Some("venue closed")));
        let mut req = request("C2");
        req.ex_destination = None;
        let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: req, redelivered: false });
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, Some(RejectReason::NoRoute));
    }

    /// `input`, a venue report, carrying the venue's OrdRejReason(103).
    fn with_ord_rej_reason(mut input: Input, code: u32) -> Input {
        let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
        report.ord_rej_reason = Some(code);
        input
    }

    #[test]
    fn exec_type_rejected_on_the_order_carries_the_venues_reason() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, with_ord_rej_reason(venue_report_as("1", ExecType::Rejected, Status::New, 0, 0), 3));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((report.reject, report.status), (Some(RejectReason::Venue(3)), Status::New));
    }

    #[test]
    fn a_reason_on_a_report_that_rejects_nothing_is_not_passed() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, with_ord_rej_reason(venue_report("1", Status::New, 0, 0), 3));
        let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(report.reject, None);
    }

    /// `input`, a cancel or replace, naming another side or symbol than the order's.
    fn misnamed(mut input: Input, other_side: bool) -> Input {
        let (Input::Cancel { side, symbol, .. } | Input::Replace { side, symbol, .. }) = &mut input else {
            unreachable!()
        };
        if other_side {
            *side = Side::Sell;
        } else {
            *symbol = "MSFT".into();
        }
        input
    }

    #[test]
    fn a_cancel_or_replace_naming_another_side_or_symbol_is_refused() {
        for other_side in [true, false] {
            for input in [cancel_input("C2", "C1"), replace_input("C2", "C1", 80, 11)] {
                let mut book = book();
                new_order(&mut book, "C1");
                let out = run(&mut book, misnamed(input, other_side));
                let [Output::CancelReject { text, reason, order_id, .. }] = out.as_slice() else { panic!("{out:?}") };
                assert_eq!(
                    (text.as_str(), *reason, *order_id),
                    ("side or symbol does not match", CancelRejectReason::Other, Some(OrderId(1)))
                );
                assert!(book.order(OrderId(1)).unwrap().pending.is_none());
                // Refused before it's numbered: the right request still goes out as the first.
                let out = cancel(&mut book, "C3", "C1");
                let [Output::Cancel { downstream, .. }] = out.as_slice() else { panic!("{out:?}") };
                assert_eq!(downstream, "1-1");
            }
        }
    }

    #[test]
    fn a_misnamed_request_for_a_done_order_is_too_late() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, venue_report("1", Status::Filled, 100, 100));
        let out = run(&mut book, misnamed(cancel_input("C2", "C1"), true));
        assert_eq!(cancel_reject(&out).3, CancelRejectReason::TooLate);
    }

    #[test]
    fn a_misnamed_replace_is_refused_before_its_own_checks() {
        let mut book = book();
        new_order(&mut book, "C1");
        replace(&mut book, "C2", "C1", 80, 11);
        let out = run(&mut book, misnamed(replace_input("C3", "C1", 70, 11), false));
        let [Output::CancelReject { text, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(text, "side or symbol does not match");
    }

    /// A replace to a Market IOC order.
    fn to_market_ioc(mut input: Input) -> Input {
        let Input::Replace { ord_type, time_in_force, price, .. } = &mut input else { unreachable!() };
        (*ord_type, *time_in_force, *price) = (Some(OrdType::Market), Some(TimeInForce::Ioc), None);
        input
    }

    fn market_ioc(cl_ord_id: &str, qty: i64) -> OrderRequest {
        OrderRequest {
            ord_type: OrdType::Market,
            time_in_force: Some(TimeInForce::Ioc),
            price: None,
            ..replaced(cl_ord_id, qty, 0)
        }
    }

    #[test]
    fn a_replace_changing_ord_type_and_time_in_force_applies_them_on_acceptance() {
        let mut book = book();
        new_order(&mut book, "C1");
        let out = run(&mut book, to_market_ioc(replace_input("C2", "C1", 80, 0)));
        let [Output::Replace { request, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(request, &market_ioc("C1", 80));
        assert_eq!(book.order(OrderId(1)).unwrap().request, self::request("C1"));
        run(&mut book, venue_report("1-1", Status::PendingReplace, 0, 0));
        assert_eq!(book.order(OrderId(1)).unwrap().request, self::request("C1"));
        run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
        assert_eq!(book.order(OrderId(1)).unwrap().request, market_ioc("C2", 80));
    }

    #[test]
    fn a_replace_to_market_drops_the_price() {
        let mut book = book();
        new_order(&mut book, "C1");
        // A price on a Market replace is the edge's to refuse; the book doesn't send one.
        let mut input = to_market_ioc(replace_input("C2", "C1", 80, 0));
        let Input::Replace { price, .. } = &mut input else { unreachable!() };
        *price = Some(Decimal::from(11));
        let out = run(&mut book, input);
        let [Output::Replace { request, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(request.price, None);
    }

    #[test]
    fn a_rejected_replace_leaves_ord_type_and_time_in_force() {
        let mut book = book();
        new_order(&mut book, "C1");
        run(&mut book, to_market_ioc(replace_input("C2", "C1", 80, 0)));
        run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
        assert_eq!(book.order(OrderId(1)).unwrap().request, request("C1"));
    }
}
