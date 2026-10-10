//! The order book: a sans-IO state machine. Each [`Input`] changes the book and pushes what to
//! send onto `out`; the FIX side turns those into typed messages for each session's version.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use turbojet::SessionId;
use turbojet::fields::{CompactString, Decimal, format_compact};

use crate::risk;
use crate::router::{Router, Target};

/// A client session: two sessions of one CompID (with different SubIDs, say) are two clients,
/// each with its own ClOrdIDs and reports. Shows as the session, as Turbojet shows it. Cheap to
/// clone; the gateway makes one per session and reuses it, so a message doesn't format one.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(Arc<ClientName>);

/// Compared by `session` first, which names the session whole; `comp_id` is part of it.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ClientName {
    session: String,
    comp_id: CompactString,
}

impl ClientId {
    /// The client on the acceptor's session `id`, the client being its counterparty.
    pub fn new(id: &SessionId) -> Self {
        Self(Arc::new(ClientName { session: id.to_string(), comp_id: id.target_comp_id.as_str().into() }))
    }

    /// The client's CompID, which routes match on; its sessions share it.
    pub fn comp_id(&self) -> &str {
        &self.0.comp_id
    }
}

/// The session, quoted: what the shutdown log's counts per client show.
impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0.session, f)
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.session)
    }
}

/// The FIX 4.4 session from JETBRIDGE to CompID `comp_id`, with no SubIDs: tests' usual client.
#[cfg(test)]
impl From<&str> for ClientId {
    fn from(comp_id: &str) -> Self {
        Self::new(&SessionId::new("FIX.4.4", "JETBRIDGE", comp_id))
    }
}

/// A venue session, by its name in the sessions file.
pub type VenueId = CompactString;

/// ClOrdIDs a client may use in one trading day, originals and amendments together. The index
/// is in memory, so it has a bound; a busy client sends tens of thousands of orders a day.
pub const MAX_CL_ORD_IDS_PER_CLIENT: usize = 1_000_000;

/// ClOrdIDs past [`MAX_CL_ORD_IDS_PER_CLIENT`] kept for cancels and replaces, so a client refused
/// new orders can still take its working orders off the market. It also keeps each order's
/// `amendments` far below `u32::MAX`.
pub const MAX_AMEND_RESERVE: usize = 10_000;

/// Times the gateway sends its own cancel for one order before it gives up and lists the order as
/// stuck in [`Book::cancelling`]. A venue that refuses a cancel three times will refuse a fourth;
/// past that, someone has to look.
pub const MAX_GATEWAY_CANCEL_TRIES: u32 = 3;

/// The start of the seconds [`start_prefix`] counts: 2026-01-01T00:00:00Z, in Unix seconds.
pub const ID_EPOCH: u64 = 1_767_225_600;

/// The book's key for an order. On the wire it's [`Order::name`], the book's prefix before it.
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
    /// The venue's session couldn't take the order: not connected, or its queue full.
    VenueUnavailable,
    /// An operator blocked the client: see [`Input::Block`].
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    /// Our OrderID(37): the book's prefix, then `id`. The root of every ID the order goes by: its
    /// ExecIDs and downstream ClOrdIDs.
    pub name: CompactString,
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
    /// The downstream ClOrdID of the latest cancel sent for the order, the client's or the
    /// gateway's: while the venue hasn't answered it, a cancel is outstanding.
    pub cancel: Option<CompactString>,
    /// The gateway's own cancel (see [`Input::CancelAll`]) while the order is live. A done order
    /// has none.
    pub gateway_cancel: Option<GatewayCancel>,
}

/// The gateway cancelling an order itself, for the kill switch or cancel on disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayCancel {
    /// Cancels the venue has seen or may yet see, at most [`MAX_GATEWAY_CANCEL_TRIES`]: one its
    /// session couldn't take doesn't count.
    pub tries: u32,
    /// Waiting to be sent: the venue's session couldn't take the last one, or a cancel already
    /// outstanding may do the job. It goes when that cancel is refused, or when the venue's
    /// session can take it (on logon or the retry tick).
    pub unsent: bool,
    /// Given up: the venue refused every try, or said it doesn't know the order or it's too late,
    /// while the book still has it live.
    pub stuck: bool,
    /// The order's downstream ClOrdID the latest cancel named: when a replace accepted since
    /// changes it, the venue's "unknown order" is worth another try.
    pub orig_downstream: CompactString,
    /// Sends the venue's session refused, for the logs: the first is a warning, repeats (a
    /// queue full tick after tick) are debug.
    pub refused_sends: u32,
}

/// Whose orders the gateway cancels, or which clients it blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One client session.
    Session(ClientId),
    /// Every session of a CompID, those that log on later too.
    CompId(CompactString),
    /// Every client.
    All,
}

impl Scope {
    fn covers(&self, client: &ClientId) -> bool {
        match self {
            Self::Session(session) => session == client,
            Self::CompId(comp_id) => client.comp_id() == comp_id,
            Self::All => true,
        }
    }
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
    /// Passed to the client as it is, except that a report with a fill (`last_qty` > 0) goes to
    /// the client as `Trade`, whatever the venue said: the book knows fills by `last_qty`, and
    /// clients by ExecType.
    pub exec_type: ExecType,
    /// OrdStatus(39); `None` keeps the order's, for reports whose status the book doesn't model
    /// (DoneForDay, Stopped, Suspended, Calculated). Such a report never rejects anything or
    /// accepts a pending replace.
    pub status: Option<Status>,
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
    /// `redelivered` as for `NewOrder`: a ClOrdID already used for the same order is answered
    /// with the order's state, under the request's own ClOrdIDs, rather than refused as a
    /// duplicate.
    Cancel {
        client: ClientId,
        cl_ord_id: CompactString,
        orig_cl_ord_id: CompactString,
        side: Side,
        symbol: CompactString,
        redelivered: bool,
    },
    /// As `Cancel`. `ord_type` and `time_in_force` are `None` to keep the order's: that's for
    /// the book's own callers, since a FIX replace carries full terms and the edge always sets
    /// both. The edge refuses a Limit replace without a price and a Market one with a price.
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
        redelivered: bool,
    },
    /// OrderStatusRequest. Only the ClOrdID identifies the order, since the index is per client;
    /// `side` and `symbol` are only for the answer about an unknown order, since FIX requires them
    /// on the request and on every ExecutionReport.
    StatusRequest {
        client: ClientId,
        cl_ord_id: CompactString,
        side: Side,
        symbol: CompactString,
    },
    VenueReport {
        venue: VenueId,
        report: VenueReport,
    },
    VenueCancelReject {
        venue: VenueId,
        downstream: CompactString,
        /// OrdStatus(39), required on OrderCancelReject; `None` for one the book doesn't model
        /// (DoneForDay, Stopped, Suspended, Calculated), which leaves the order's as for a
        /// Rejected ExecutionReport without a status.
        status: Option<Status>,
        text: Option<CompactString>,
        /// CxlRejReason(102).
        reason: Option<u32>,
        /// OrderID(37), the venue's.
        venue_order_id: Option<CompactString>,
    },
    /// The request sent under `downstream` never reached the venue's session, or the session
    /// dropped it unsent: it isn't connected, its queue is full, or the connection ended first. A
    /// client's cancel or replace is refused with `text` and the order's status left as it is,
    /// even when the order is done, since the venue never saw the request to answer it; an
    /// original order still PendingNew is rejected. The gateway's own cancel waits for `VenueUp`.
    /// One for an answered request, or an original order the venue acked or that is done, is
    /// dropped. It never produces venue-bound output, which the edge relies on.
    SendFailed {
        venue: VenueId,
        downstream: CompactString,
        text: CompactString,
    },
    /// The gateway cancels every live order in `scope` itself, except those with a cancel
    /// outstanding. The client hears only the venue's Canceled, as an unsolicited report under
    /// the order's current ClOrdID. A venue's refusal is logged and the cancel sent again, up to
    /// [`MAX_GATEWAY_CANCEL_TRIES`] times; see [`Book::cancelling`].
    CancelAll {
        scope: Scope,
    },
    /// As `CancelAll`, for one order.
    CancelOrder {
        order: OrderId,
    },
    /// The venue's session logged on, or can take requests again: the gateway's cancels that
    /// couldn't be sent to it go now.
    VenueUp {
        venue: VenueId,
    },
    /// An operator blocks the clients in `scope`: their new orders are rejected
    /// ([`RejectReason::Blocked`]) until an `Unblock` of the same scope. What takes orders off the
    /// market passes: cancels, and replaces that don't raise the quantity.
    Block {
        scope: Scope,
    },
    /// Lifts the block of exactly `scope`: unblocking a session leaves a block of its CompID, or
    /// of everyone, in force.
    Unblock {
        scope: Scope,
    },
}

/// An ExecutionReport for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientReport {
    /// The venue's, except for the book's own rejects (`Rejected`) and answers to redeliveries
    /// and status requests (`Status`).
    pub exec_type: ExecType,
    /// [`Order::name`]; `None` for an order rejected before it was booked, or a status request
    /// for none.
    pub order_id: Option<CompactString>,
    pub cl_ord_id: CompactString,
    pub orig_cl_ord_id: Option<CompactString>,
    /// For a status request about no order: the ClOrdID, side and symbol asked, Market, quantity 0.
    pub request: OrderRequest,
    pub status: Status,
    pub reject: Option<RejectReason>,
    pub last_qty: Decimal,
    pub last_px: Decimal,
    pub cum_qty: Decimal,
    pub leaves_qty: Decimal,
    pub avg_px: Decimal,
    /// ExecID(17): `<OrderID>.<n>` for a booked order, `<prefix>R<n>` for a reject before booking.
    pub exec_id: CompactString,
    pub text: Option<CompactString>,
}

/// What an OrderCancelReject answers: ResponseTo(434).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelKind {
    Cancel,
    Replace,
}

/// CxlRejReason(102): 0, 1, 3, 6 and 99, or the venue's; the FIX side maps it per version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRejectReason {
    TooLate,
    UnknownOrder,
    /// 3: a replace is already pending for the order, or the gateway's own cancel.
    AlreadyPending,
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
        order_id: Option<CompactString>,
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

/// Who asked for a request we sent a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Client,
    /// The gateway's own cancel: it spends none of the client's ClOrdIDs.
    Gateway,
}

/// A request we sent a venue, by its downstream ClOrdID: what the venue's answers map back to.
#[derive(Debug, Clone)]
struct Sent {
    order: OrderId,
    origin: Origin,
    /// The client's ClOrdID for the request, and its OrigClOrdID for a cancel or replace. Empty
    /// and `None` for the gateway's own cancel, whose answers go under the order's current one.
    cl_ord_id: CompactString,
    orig_cl_ord_id: Option<CompactString>,
    /// `None` for the original order.
    kind: Option<CancelKind>,
    /// The venue has rejected or accepted it (a cancel's Canceled, a replace's acceptance): a
    /// later reject is a resend.
    answered: bool,
}

/// Every ID the book issues starts with its prefix: OrderIDs, the ExecIDs `<OrderID>.<n>` and
/// `<prefix>R<n>`, and the downstream ClOrdIDs `<OrderID>` and `<OrderID>-<n>`. The numbering
/// starts again at each start, and a venue's late report or resend about an order from before a
/// restart must not land on this start's order of the same number, so the gateway sets the
/// prefix from its start time ([`start_prefix`]). A report under an ID without this start's
/// prefix is for an unknown order.
#[derive(Debug)]
pub struct Book {
    router: Router,
    limits: risk::Config,
    /// Empty unless set by [`set_prefix`](Self::set_prefix).
    prefix: CompactString,
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
    /// Operators' blocks, each scope once: a handful, so a list.
    blocked: Vec<Scope>,
    /// A refused send left a gateway cancel waiting since [`take_cancel_unsent`](Self::take_cancel_unsent).
    cancel_unsent: bool,
}

impl Book {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            limits: risk::Config::default(),
            prefix: CompactString::default(),
            orders: HashMap::new(),
            index: HashMap::new(),
            used: HashMap::new(),
            downstream: HashMap::new(),
            next_id: 1,
            rejects: 0,
            max_cl_ord_ids: MAX_CL_ORD_IDS_PER_CLIENT,
            amend_reserve: MAX_AMEND_RESERVE,
            blocked: Vec::new(),
            cancel_unsent: false,
        }
    }

    /// Starts every ID the book issues with `prefix`; see [`Book`]. Set it before the book
    /// issues any.
    pub fn set_prefix(&mut self, prefix: CompactString) {
        debug_assert!(self.next_id == 1 && self.rejects == 0, "the prefix changed after the book issued IDs");
        self.prefix = prefix;
    }

    pub fn prefix(&self) -> &CompactString {
        &self.prefix
    }

    /// Routes and checks orders by these from the next order on; orders already routed keep
    /// their venue.
    pub fn set_config(&mut self, router: Router, limits: risk::Config) {
        self.router = router;
        self.limits = limits;
    }

    pub fn router(&self) -> &Router {
        &self.router
    }

    pub fn limits(&self) -> &risk::Config {
        &self.limits
    }

    /// Orders not yet done, counted per venue.
    pub fn open_orders_by_venue(&self) -> BTreeMap<VenueId, usize> {
        let mut open = BTreeMap::new();
        for order in self.orders.values().filter(|order| !order.status.is_done()) {
            *open.entry(order.venue.clone()).or_default() += 1;
        }
        open
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    pub fn handle(&mut self, input: Input, out: &mut Vec<Output>) {
        match input {
            Input::NewOrder { client, request, redelivered } => self.new_order(client, request, redelivered, out),
            Input::StatusRequest { client, cl_ord_id, side, symbol } => {
                self.status_request(client, cl_ord_id, side, symbol, out)
            }
            Input::VenueReport { venue, report } => self.venue_report(&venue, report, out),
            Input::Cancel { client, cl_ord_id, orig_cl_ord_id, side, symbol, redelivered } => {
                let amend = Amend { client, cl_ord_id, orig_cl_ord_id, kind: CancelKind::Cancel, redelivered };
                self.cancel(amend, (side, symbol), out)
            }
            Input::Replace {
                client,
                cl_ord_id,
                orig_cl_ord_id,
                side,
                symbol,
                qty,
                price,
                ord_type,
                time_in_force,
                redelivered,
            } => {
                let amend = Amend { client, cl_ord_id, orig_cl_ord_id, kind: CancelKind::Replace, redelivered };
                let terms = Terms { qty, price, ord_type, time_in_force };
                self.replace(amend, (side, symbol), terms, out)
            }
            Input::VenueCancelReject { venue, downstream, status, text, reason, venue_order_id } => {
                let refused = Refused { status, text, venue_order_id, reason };
                self.venue_cancel_reject(&venue, &downstream, refused, out)
            }
            Input::SendFailed { venue, downstream, text } => self.send_failed(&venue, &downstream, text, out),
            Input::CancelAll { scope } => self.cancel_all(&scope, out),
            Input::CancelOrder { order } => self.cancel_order(order, out),
            Input::VenueUp { venue } => self.venue_up(&venue, out),
            Input::Block { scope } => {
                if !self.blocked.contains(&scope) {
                    self.blocked.push(scope);
                }
            }
            Input::Unblock { scope } => self.blocked.retain(|blocked| *blocked != scope),
        }
        #[cfg(debug_assertions)]
        self.check();
    }

    /// The book's invariants, after every input. It walks every order and request sent, so a
    /// debug build slows as the book grows, O(book) per message: don't load-test one. Release
    /// builds leave it out.
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
        for sent in self.downstream.values() {
            let order = &self.orders[&sent.order];
            match sent.origin {
                Origin::Client => {
                    assert_eq!(self.index.get(&(order.client.clone(), sent.cl_ord_id.clone())), Some(&sent.order))
                }
                Origin::Gateway => {
                    assert!(
                        sent.cl_ord_id.is_empty() && sent.orig_cl_ord_id.is_none(),
                        "a gateway cancel uses no client ClOrdID"
                    );
                    assert_eq!(sent.kind, Some(CancelKind::Cancel));
                }
            }
        }
        for (id, order) in &self.orders {
            if let Some(cancel) = &order.cancel {
                let sent = &self.downstream[cancel];
                assert_eq!((sent.order, sent.kind), (*id, Some(CancelKind::Cancel)));
            }
            if let Some(gateway) = &order.gateway_cancel {
                assert!(!order.status.is_done(), "done order {id:?} has a gateway cancel");
                assert!(gateway.tries <= MAX_GATEWAY_CANCEL_TRIES, "{id:?}'s gateway cancel tries");
                let in_flight = self.gateway_cancel_in_flight(order);
                // Exactly one of: waiting to be sent, in flight, given up.
                assert_eq!(
                    u8::from(gateway.unsent) + u8::from(in_flight) + u8::from(gateway.stuck),
                    1,
                    "{id:?}: {gateway:?}"
                );
                assert!(gateway.tries > 0 || gateway.unsent, "{id:?}'s gateway cancel");
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
                Output::Report { client, report: order.status_report(request.cl_ord_id) }
            } else {
                self.reject(client, request, RejectReason::Duplicate, "duplicate ClOrdID".into())
            };
            return out.push(output);
        }
        if self.is_blocked(&client) {
            return out.push(self.reject(client, request, RejectReason::Blocked, "blocked by an operator".into()));
        }
        if self.used.get(&client).is_some_and(|&used| used >= self.max_cl_ord_ids) {
            return out.push(self.reject(client, request, RejectReason::TooManyOrders, "too many orders today".into()));
        }
        let venue = match self.router.route(client.comp_id(), &request) {
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
        let name = format_compact!("{}{}", self.prefix, id.0);
        let downstream = name.clone();
        self.remember(&client, request.cl_ord_id.clone(), id);
        let sent = Sent {
            order: id,
            origin: Origin::Client,
            cl_ord_id: request.cl_ord_id.clone(),
            orig_cl_ord_id: None,
            kind: None,
            answered: false,
        };
        let previous = self.downstream.insert(downstream.clone(), sent);
        debug_assert!(previous.is_none(), "downstream ClOrdID {downstream} reused");
        out.push(Output::NewOrder { venue: venue.clone(), downstream: downstream.clone(), request: request.clone() });
        let order = Order {
            id,
            name,
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
            cancel: None,
            gateway_cancel: None,
        };
        let previous = self.orders.insert(id, order);
        debug_assert!(previous.is_none(), "order {id:?} booked twice");
    }

    /// The order's state, or for a ClOrdID the client never used, a Rejected report saying so:
    /// OrdStatus 8 is FIX's answer for an unknown order.
    fn status_request(
        &mut self,
        client: ClientId,
        cl_ord_id: CompactString,
        side: Side,
        symbol: CompactString,
        out: &mut Vec<Output>,
    ) {
        if let Some(id) = self.index.get(&(client.clone(), cl_ord_id.clone())) {
            let order = self.orders.get_mut(id).expect("indexed ClOrdIDs point at booked orders");
            return out.push(Output::Report { client, report: order.status_report(cl_ord_id) });
        }
        // The client named only the ClOrdID, side and symbol; the rest says nothing is there.
        let request = OrderRequest {
            cl_ord_id,
            account: None,
            symbol,
            side,
            ord_type: OrdType::Market,
            qty: Decimal::ZERO,
            price: None,
            ex_destination: None,
            time_in_force: None,
        };
        let report = self.unbooked_report(request, "unknown order".into());
        out.push(Output::Report { client, report });
    }

    fn venue_report(&mut self, venue: &VenueId, mut report: VenueReport, out: &mut Vec<Output>) {
        let Some(sent) = self.downstream.get(&report.downstream).cloned() else {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "execution report for an unknown order");
        };
        // Rejected under a cancel's or replace's ClOrdID is about that request, not the order.
        // Some venues refuse an amendment with ExecType=8 and the order's live OrdStatus: that
        // status stands, as an OrderCancelReject's would.
        let rejected =
            report.status.is_some_and(|status| status == Status::Rejected || report.exec_type == ExecType::Rejected);
        if rejected && sent.kind.is_some() {
            let status = report.status.filter(|&status| status != Status::Rejected);
            // An ExecutionReport has no CxlRejReason: the refusal's reason is Other.
            let refused = Refused { status, text: report.text, venue_order_id: report.venue_order_id, reason: None };
            let id = sent.order;
            self.refuse_request(venue, &report.downstream, sent, refused, out);
            return self.resume_gateway_cancel(id, out);
        }
        let Sent { order: id, origin, cl_ord_id, orig_cl_ord_id, kind, answered } = sent;
        let order = self.orders.get_mut(&id).expect("downstream ClOrdIDs point at booked orders");
        if order.venue != *venue {
            return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "execution report for another venue's order");
        }
        if stale(order, answered, &report) {
            // A restatement is the venue changing the order: losing one needs a look.
            if report.exec_type == ExecType::Restated {
                return tracing::warn!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "restatement dropped during a pending request");
            }
            return tracing::debug!(%venue, downstream = %report.downstream, exec_id = %report.exec_id, "stale execution report dropped");
        }
        let pending_here = order.pending.as_ref().is_some_and(|pending| pending.downstream == report.downstream);
        // Any answer under the replace's ClOrdID but these means the venue took it.
        let accepted =
            pending_here && !rejected && report.status.is_some_and(|status| status != Status::PendingReplace);
        // A client's cancel's reports and a pending replace's (up to the one accepting it) answer
        // that request; any other, the gateway's cancel's too, is about the order, unsolicited.
        let answers_request = match kind {
            Some(CancelKind::Cancel) => origin == Origin::Client,
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
        order.record_venue_order_id(report.venue_order_id.take(), accepted);
        if accepted {
            // The downstream the replace supersedes is settled like any other: its late reports
            // without a fill are resends.
            let superseded = order.apply_replace();
            self.downstream.get_mut(&superseded).expect("an order's downstream is recorded").answered = true;
        }
        if accepted || (kind == Some(CancelKind::Cancel) && report.status == Some(Status::Canceled)) {
            self.downstream.get_mut(&report.downstream).expect("looked up above").answered = true;
        }
        order.cum_qty = report.cum_qty;
        order.avg_px = report.avg_px;
        // `None` keeps the status, through `set_status` so a fill makes a New order PartiallyFilled.
        order.set_status(report.status.unwrap_or(order.status));
        // The gateway's cancel's acks aren't the client's: after a refusal it would be left
        // showing PendingCancel. Fills and the end are.
        if origin == Origin::Gateway && report.last_qty == Decimal::ZERO && !order.status.is_done() {
            return tracing::debug!(%venue, downstream = %report.downstream, "gateway cancel's ack not forwarded");
        }
        let answers = answers_request.then_some((cl_ord_id, orig_cl_ord_id));
        let client_report = order.client_report(answers, report, rejected);
        out.push(Output::Report { client: order.client.clone(), report: client_report });
    }

    /// A cancel goes to the venue even while PendingNew, under the next numbered downstream ClOrdID.
    /// One is refused while the gateway's own cancel is in flight: the order is going already, and
    /// the client hears when it's gone. A stuck one, or one waiting for the venue's session to take it, lets
    /// the client's through, which may fare better.
    fn cancel(&mut self, amend: Amend, named: (Side, CompactString), out: &mut Vec<Output>) {
        let refuse = |book: &Self, order: &Order| {
            book.gateway_cancel_in_flight(order)
                .then_some((CancelRejectReason::AlreadyPending, "the gateway is cancelling the order"))
        };
        let Some((id, amend)) = self.amendable(amend, named, refuse, out) else { return };
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
    /// the venue accepts it (see `venue_report`). Refused, like a cancel, while the gateway's own
    /// cancel is in flight, and for a blocked client, if it raises the quantity.
    fn replace(&mut self, amend: Amend, named: (Side, CompactString), terms: Terms, out: &mut Vec<Output>) {
        let Terms { qty, price, ord_type, time_in_force } = terms;
        let refuse = |book: &Self, order: &Order| {
            if book.gateway_cancel_in_flight(order) {
                Some((CancelRejectReason::AlreadyPending, "the gateway is cancelling the order"))
            } else if order.pending.is_some() {
                Some((CancelRejectReason::AlreadyPending, "replace pending"))
            } else if qty < order.cum_qty {
                Some((CancelRejectReason::Other, "quantity below filled quantity"))
            } else if qty > order.request.qty && book.is_blocked(&order.client) {
                Some((CancelRejectReason::Other, "blocked: a replace may not raise the quantity"))
            } else {
                None
            }
        };
        let Some((id, amend)) = self.amendable(amend, named, refuse, out) else { return };
        let cl_ord_id = amend.cl_ord_id.clone();
        let (downstream, order) = self.number(id, amend);
        let ord_type = ord_type.unwrap_or(order.request.ord_type);
        let time_in_force = time_in_force.or(order.request.time_in_force);
        debug_assert!(
            (ord_type == OrdType::Limit) == price.is_some(),
            "the edge refuses a Limit replace without a price and a Market one with a price"
        );
        // A market order has no price: the release fallback should the contract slip.
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

    /// Whether a refused send has left a gateway cancel waiting since the last call: the edge's
    /// retry tick looks for waiting cancels only then.
    pub fn take_cancel_unsent(&mut self) -> bool {
        std::mem::take(&mut self.cancel_unsent)
    }

    /// The scopes operators have blocked, in the order they were blocked.
    pub fn blocked(&self) -> &[Scope] {
        &self.blocked
    }

    fn is_blocked(&self, client: &ClientId) -> bool {
        self.blocked.iter().any(|scope| scope.covers(client))
    }

    /// The live order a cancel or replace names, or `None` with an OrderCancelReject refusing it
    /// pushed, or the order's state answering a resend of a request already taken. `refuse` adds
    /// the request's own checks after the shared ones, the last of which is that the request
    /// `named` the order's side and symbol.
    fn amendable(
        &mut self,
        amend: Amend,
        named: (Side, CompactString),
        refuse: impl Fn(&Self, &Order) -> Option<(CancelRejectReason, &'static str)>,
        out: &mut Vec<Output>,
    ) -> Option<(OrderId, Amend)> {
        let id = self.index.get(&(amend.client.clone(), amend.orig_cl_ord_id.clone())).copied();
        // Before `remember`, which asserts the ClOrdID is new.
        let used_for = self.index.get(&(amend.client.clone(), amend.cl_ord_id.clone())).copied();
        if let Some(id) = id.filter(|&id| amend.redelivered && used_for == Some(id)) {
            // Side and symbol aren't checked again: the ClOrdID already maps to this order.
            let order = self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders");
            // It answers that request, under its own ClOrdIDs, as the venue's reports for it do.
            let mut report = order.report(&amend.cl_ord_id, ExecType::Status);
            report.orig_cl_ord_id = Some(amend.orig_cl_ord_id);
            out.push(Output::Report { client: amend.client, report });
            return None;
        }
        let order = id.map(|id| self.orders.get(&id).expect("indexed ClOrdIDs point at booked orders"));
        let duplicate = used_for.is_some();
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
            Some(order) if (order.request.side, &order.request.symbol) != (named.0, &named.1) => {
                Some((CancelRejectReason::Other, "side or symbol does not match"))
            }
            Some(order) => refuse(self, order),
        };
        if let Some((reason, text)) = refusal {
            out.push(amend.refuse(order, reason, text.into()));
            return None;
        }
        Some((order.expect("an unknown order is refused").id, amend))
    }

    /// Sends `amend` under the order's next numbered downstream ClOrdID, remembering what it was.
    fn number(&mut self, id: OrderId, amend: Amend) -> (CompactString, &mut Order) {
        let sent = Sent {
            order: id,
            origin: Origin::Client,
            cl_ord_id: amend.cl_ord_id.clone(),
            orig_cl_ord_id: Some(amend.orig_cl_ord_id),
            kind: Some(amend.kind),
            answered: false,
        };
        let downstream = self.next_downstream(sent);
        self.remember(&amend.client, amend.cl_ord_id, id);
        (downstream, self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders"))
    }

    /// The order's next numbered downstream ClOrdID, recording `sent` under it.
    fn next_downstream(&mut self, sent: Sent) -> CompactString {
        let order = self.orders.get_mut(&sent.order).expect("requests are for booked orders");
        order.amendments += 1;
        let downstream = format_compact!("{}-{}", order.name, order.amendments);
        if sent.kind == Some(CancelKind::Cancel) {
            order.cancel = Some(downstream.clone());
        }
        let previous = self.downstream.insert(downstream.clone(), sent);
        debug_assert!(previous.is_none(), "downstream ClOrdID {downstream} reused");
        downstream
    }

    fn gateway_cancel_in_flight(&self, order: &Order) -> bool {
        self.outstanding_cancel(order).is_some_and(|sent| sent.origin == Origin::Gateway)
    }

    /// The order's latest cancel, while the venue hasn't answered it.
    fn outstanding_cancel(&self, order: &Order) -> Option<&Sent> {
        let sent = &self.downstream[order.cancel.as_ref()?];
        (!sent.answered).then_some(sent)
    }

    fn cancel_all(&mut self, scope: &Scope, out: &mut Vec<Output>) {
        let mut ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|order| scope.covers(&order.client) && !order.status.is_done())
            .map(|order| order.id)
            .collect();
        // Map order is random; a fixed one keeps the venues' traffic and the tests repeatable.
        ids.sort_unstable_by_key(|id| id.0);
        for id in ids {
            self.cancel_order(id, out);
        }
    }

    /// Starts the gateway's own cancel for the order, the tries counted afresh: an operator asking
    /// again gets a stuck order another round. One in flight is left alone; one waits behind the
    /// client's cancel outstanding, going if the venue refuses that.
    fn cancel_order(&mut self, id: OrderId, out: &mut Vec<Output>) {
        let Some(order) = self.orders.get(&id) else {
            return tracing::warn!(order = id.0, "gateway cancel for an unknown order");
        };
        if order.status.is_done() {
            return tracing::debug!(order = %order.name, "gateway cancel for a done order");
        }
        let outstanding = self.outstanding_cancel(order).map(|sent| sent.origin);
        if outstanding == Some(Origin::Gateway) {
            return tracing::debug!(order = %order.name, "gateway cancel already in flight");
        }
        let order = self.orders.get_mut(&id).expect("looked up above");
        let orig_downstream = order.downstream.clone();
        order.gateway_cancel =
            Some(GatewayCancel { tries: 0, unsent: true, stuck: false, orig_downstream, refused_sends: 0 });
        if outstanding.is_some() {
            return tracing::info!(order = %order.name, "gateway cancel waits on the client's");
        }
        self.send_gateway_cancel(id, out);
    }

    /// Sends the gateway's cancel for the order one more time.
    fn send_gateway_cancel(&mut self, id: OrderId, out: &mut Vec<Output>) {
        let sent = Sent {
            order: id,
            origin: Origin::Gateway,
            cl_ord_id: CompactString::default(),
            orig_cl_ord_id: None,
            kind: Some(CancelKind::Cancel),
            answered: false,
        };
        let downstream = self.next_downstream(sent);
        let order = self.orders.get_mut(&id).expect("requests are for booked orders");
        let gateway = order.gateway_cancel.as_mut().expect("the caller started the gateway's cancel");
        debug_assert!(gateway.tries < MAX_GATEWAY_CANCEL_TRIES && !gateway.stuck);
        gateway.tries += 1;
        gateway.unsent = false;
        gateway.orig_downstream = order.downstream.clone();
        tracing::info!(order = %order.name, %downstream, tries = gateway.tries, "gateway cancelling an order");
        out.push(Output::Cancel {
            venue: order.venue.clone(),
            downstream,
            orig_downstream: order.downstream.clone(),
            request: order.request.clone(),
            venue_order_id: order.venue_order_id.clone(),
        });
    }

    /// Sends the gateway's cancels for the venue that are waiting, with no cancel outstanding.
    fn venue_up(&mut self, venue: &VenueId, out: &mut Vec<Output>) {
        let mut ids: Vec<OrderId> = self
            .orders
            .values()
            .filter(|order| order.venue == *venue && self.gateway_cancel_waiting(order))
            .map(|order| order.id)
            .collect();
        ids.sort_unstable_by_key(|id| id.0);
        for id in ids {
            self.send_gateway_cancel(id, out);
        }
    }

    /// The gateway's cancel is waiting to be sent, and nothing stands in its way.
    fn gateway_cancel_waiting(&self, order: &Order) -> bool {
        order.gateway_cancel.as_ref().is_some_and(|gateway| gateway.unsent) && self.outstanding_cancel(order).is_none()
    }

    /// After the venue refused a request for the order: the gateway's cancel that waited on a
    /// client's goes now.
    fn resume_gateway_cancel(&mut self, id: OrderId, out: &mut Vec<Output>) {
        if self.gateway_cancel_waiting(&self.orders[&id]) {
            self.send_gateway_cancel(id, out);
        }
    }

    /// Sends the gateway's cancel again, or marks the order stuck once it has had its tries.
    fn retry_gateway_cancel(&mut self, id: OrderId, out: &mut Vec<Output>) {
        let order = self.orders.get_mut(&id).expect("requests are for booked orders");
        let gateway = order.gateway_cancel.as_mut().expect("the caller checked for the gateway's cancel");
        if gateway.tries >= MAX_GATEWAY_CANCEL_TRIES {
            gateway.stuck = true;
            return tracing::error!(order = %order.name, tries = gateway.tries, "gateway gave up cancelling an order");
        }
        self.send_gateway_cancel(id, out);
    }

    /// Orders the gateway is cancelling itself, by OrderId: what the shutdown log and the console
    /// show. A stuck one needs someone to look.
    pub fn cancelling(&self) -> Vec<(OrderId, GatewayCancel)> {
        let mut cancelling: Vec<_> =
            self.orders.values().filter_map(|order| Some((order.id, order.gateway_cancel.clone()?))).collect();
        cancelling.sort_unstable_by_key(|(id, _)| id.0);
        cancelling
    }

    /// Whether every cancel the gateway sent itself is answered or given up (stuck).
    pub fn cancels_settled(&self) -> bool {
        !self.orders.values().any(|order| order.gateway_cancel.as_ref().is_some_and(|cancel| !cancel.stuck))
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
        let id = sent.order;
        self.refuse_request(venue, downstream, sent, refused, out);
        self.resume_gateway_cancel(id, out);
    }

    /// The venue's session couldn't take what we sent under `downstream`, so the venue never saw
    /// it. A cancel or replace is refused, leaving the order's status as it is; an original order
    /// still PendingNew is rejected.
    fn send_failed(&mut self, venue: &VenueId, downstream: &CompactString, text: CompactString, out: &mut Vec<Output>) {
        let Some(sent) = self.downstream.get(downstream).cloned() else {
            return tracing::warn!(%venue, %downstream, "send failed for an unknown request");
        };
        let order = self.orders.get_mut(&sent.order).expect("downstream ClOrdIDs point at booked orders");
        debug_assert_eq!(order.venue, *venue, "the edge reports a failed send to the order's venue");
        if order.venue != *venue {
            return tracing::warn!(%venue, %downstream, "send failed for another venue's order");
        }
        // The gateway's own cancel waits until the venue's session can take it (on logon or the
        // edge's retry tick): nothing to send now, which the edge relies on.
        if sent.origin == Origin::Gateway {
            if sent.answered {
                return tracing::debug!(%venue, %downstream, "send failed for an answered request; dropped");
            }
            self.downstream.get_mut(downstream).expect("looked up above").answered = true;
            // Done since, the order needs no more cancelling.
            let Some(gateway) = &mut order.gateway_cancel else {
                return tracing::debug!(%venue, %downstream, "send failed for a gateway cancel of a done order");
            };
            debug_assert!(gateway.tries > 0 && !gateway.unsent && !gateway.stuck);
            gateway.tries -= 1;
            gateway.unsent = true;
            gateway.refused_sends = gateway.refused_sends.saturating_add(1);
            self.cancel_unsent = true;
            if gateway.refused_sends > 1 {
                return tracing::debug!(%venue, %downstream, %text, refused = gateway.refused_sends, "gateway cancel not sent again");
            }
            return tracing::warn!(%venue, %downstream, %text, "gateway cancel not sent; again when the venue's session can take it");
        }
        // A cancel or replace the venue never saw is refused even on a done order: nobody else
        // will answer it. `refuse_request` drops one already answered.
        if sent.kind.is_some() {
            let refused = Refused { status: Some(order.status), text: Some(text), venue_order_id: None, reason: None };
            return self.refuse_request(venue, downstream, sent, refused, out);
        }
        // A venue's report means the original got there; done is final, as in `stale`.
        if sent.answered || order.status != Status::PendingNew {
            return tracing::debug!(%venue, %downstream, "send failed for an answered order; dropped");
        }
        self.downstream.get_mut(downstream).expect("looked up above").answered = true;
        order.set_status(Status::Rejected);
        let mut report = order.report(&order.request.cl_ord_id.clone(), ExecType::Rejected);
        report.reject = Some(RejectReason::VenueUnavailable);
        report.text = Some(text);
        out.push(Output::Report { client: order.client.clone(), report });
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
        let Some(response_to) = sent.kind else {
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
        if sent.origin == Origin::Gateway {
            return self.gateway_cancel_refused(sent.order, downstream, reason, text, out);
        }
        let orig_cl_ord_id = sent.orig_cl_ord_id.expect("a client's cancel or replace names an order");
        let client = order.client.clone();
        let amend = Amend { client, cl_ord_id: sent.cl_ord_id, orig_cl_ord_id, kind: response_to, redelivered: false };
        let text = text.unwrap_or_else(|| "rejected by the venue".into());
        let reason = reason.map_or(CancelRejectReason::Other, CancelRejectReason::Venue);
        out.push(amend.refuse(Some(&*order), reason, text));
    }

    /// The venue refused the gateway's own cancel: the client never hears of it. Sent again,
    /// unless the venue says it's too late or doesn't know the order, which another try won't
    /// change; an order that's live here all the same is stuck.
    fn gateway_cancel_refused(
        &mut self,
        id: OrderId,
        downstream: &CompactString,
        reason: Option<u32>,
        text: Option<CompactString>,
        out: &mut Vec<Output>,
    ) {
        let order = self.orders.get_mut(&id).expect("downstream ClOrdIDs point at booked orders");
        let text = text.unwrap_or_default();
        // The refusal's status made the order done, which ends the gateway's cancel.
        let Some(gateway) = order.gateway_cancel.as_mut() else {
            return tracing::info!(order = %order.name, %downstream, ?reason, %text, "venue refused the gateway's cancel of a done order");
        };
        tracing::error!(order = %order.name, %downstream, ?reason, %text, tries = gateway.tries, "venue refused the gateway's cancel");
        // CxlRejReason 0 is too late, 1 unknown order: unless a replace accepted since gave the
        // order a new downstream ClOrdID, which the venue may yet know.
        if matches!(reason, Some(0 | 1)) && order.downstream == gateway.orig_downstream {
            gateway.stuck = true;
            return tracing::error!(order = %order.name, "gateway gave up cancelling an order the venue says is done or unknown");
        }
        self.retry_gateway_cancel(id, out);
    }

    /// A reject for an order we didn't book.
    fn reject(&mut self, client: ClientId, request: OrderRequest, reason: RejectReason, text: CompactString) -> Output {
        let report =
            ClientReport { exec_type: ExecType::Rejected, reject: Some(reason), ..self.unbooked_report(request, text) };
        Output::Report { client, report }
    }

    /// A Rejected `Status` report about no order of ours, under the book's next `R` ExecID.
    fn unbooked_report(&mut self, request: OrderRequest, text: CompactString) -> ClientReport {
        // Saturating, as in `Order::report`; a u64 of rejects can't be reached.
        self.rejects = self.rejects.saturating_add(1);
        ClientReport {
            exec_type: ExecType::Status,
            order_id: None,
            cl_ord_id: request.cl_ord_id.clone(),
            orig_cl_ord_id: None,
            request,
            status: Status::Rejected,
            reject: None,
            last_qty: Decimal::ZERO,
            last_px: Decimal::ZERO,
            cum_qty: Decimal::ZERO,
            leaves_qty: Decimal::ZERO,
            avg_px: Decimal::ZERO,
            exec_id: format_compact!("{}R{}", self.prefix, self.rejects),
            text: Some(text),
        }
    }

    fn remember(&mut self, client: &ClientId, cl_ord_id: CompactString, id: OrderId) {
        let previous = self.index.insert((client.clone(), cl_ord_id), id);
        debug_assert!(previous.is_none(), "ClOrdID reused by {client}");
        *self.used.entry(client.clone()).or_default() += 1;
    }
}

/// The prefix for a book started at `start`: the seconds since [`ID_EPOCH`] in base 36, upper
/// case, zero-padded to six digits (enough until 2095). The fixed width keeps IDs from
/// different starts apart with no separator, which would cost a character of a venue's ClOrdID
/// limit and could be read as one of the `-<n>` and `.<n>` suffixes. Digits and upper-case
/// letters, so a venue that ignores case or allows only alphanumerics takes it. Two starts in
/// the same second share it; a clock before 2026 gives "000000".
pub fn start_prefix(start: SystemTime) -> CompactString {
    const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let unix = start.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let mut secs = unix.saturating_sub(ID_EPOCH);
    let mut digits = Vec::new();
    while secs > 0 || digits.len() < 6 {
        digits.push(DIGITS[(secs % 36) as usize]);
        secs /= 36;
    }
    digits.iter().rev().map(|&digit| char::from(digit)).collect()
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
            order_id: Some(self.name.clone()),
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
            exec_id: format_compact!("{}.{}", self.name, self.exec_seq),
            text: None,
        }
    }

    /// The client's report of the venue's `report`: under the ClOrdIDs of the request it
    /// `answers`, if any, else under the order's current ClOrdID.
    fn client_report(
        &mut self,
        answers: Option<(CompactString, Option<CompactString>)>,
        report: VenueReport,
        rejected: bool,
    ) -> ClientReport {
        // Clients count fills by ExecType (FIX 4.2 has nothing else), so every fill is a Trade.
        let exec_type = if report.last_qty > Decimal::ZERO { ExecType::Trade } else { report.exec_type };
        let mut client_report = match answers {
            Some((cl_ord_id, orig_cl_ord_id)) => {
                let mut client_report = self.report(&cl_ord_id, exec_type);
                client_report.orig_cl_ord_id = orig_cl_ord_id;
                client_report
            }
            None => self.report(&self.request.cl_ord_id.clone(), exec_type),
        };
        client_report.last_qty = report.last_qty;
        client_report.last_px = report.last_px;
        client_report.text = report.text;
        if rejected {
            client_report.reject = report.ord_rej_reason.map(RejectReason::Venue);
        }
        client_report
    }

    /// Takes the pending replace's terms, the venue having accepted it, returning the downstream
    /// ClOrdID it supersedes.
    fn apply_replace(&mut self) -> CompactString {
        let pending = self.pending.take().expect("a replace is pending");
        self.request.cl_ord_id = pending.cl_ord_id;
        self.request.qty = pending.qty;
        self.request.price = pending.price;
        self.request.ord_type = pending.ord_type;
        self.request.time_in_force = pending.time_in_force;
        std::mem::replace(&mut self.downstream, pending.downstream)
    }

    /// Where the order stands, answering a client asking by `asked`: a status request or a
    /// redelivered new order. Reports carry the order's latest ClOrdID; after a replace, OrigClOrdID
    /// names the one the client asked about.
    fn status_report(&mut self, asked: CompactString) -> ClientReport {
        let mut report = self.report(&self.request.cl_ord_id.clone(), ExecType::Status);
        if report.cl_ord_id != asked {
            report.orig_cl_ord_id = Some(asked);
        }
        report
    }

    /// A done order has nothing pending: a replace it still awaited can't happen now, and the
    /// gateway has nothing left to cancel. Set after `cum_qty`: a venue's New on an order with
    /// fills (a cancel reject's OrdStatus, say) is PartiallyFilled to the client.
    fn set_status(&mut self, status: Status) {
        self.status = match status {
            Status::New | Status::PendingNew if self.cum_qty > Decimal::ZERO => Status::PartiallyFilled,
            status => status,
        };
        if self.status.is_done() {
            self.pending = None;
            self.gateway_cancel = None;
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
/// would roll the order back. `answered` is whether the venue has settled the request the report
/// comes under. Dropped:
///
/// - anything for a done order (busts and corrections come later);
/// - a CumQty below the order's, or a fill at the order's CumQty (a resent fill).
///
/// Other fills always apply. Without a fill:
///
/// - (a) `settled`: under a settled request, unless it's the order's current downstream, where
///   only the accepted replace's own PendingReplace is a resend (the order's later reports come
///   under it too). A done status is exempt: it can't roll anything back, and dropping it would
///   leave an order live here that the venue has ended;
/// - (b) `behind_new`: PendingNew once the order is past it;
/// - (c) `behind_pending`: New or PartiallyFilled while the order is PendingCancel or
///   PendingReplace, unless it accepts the pending replace: the venue refuses a request with an
///   OrderCancelReject, which comes another way.
///
/// A pending state may follow PartiallyFilled, so status alone doesn't order reports. Fills are
/// known by LastQty, not status, since a fill on an order pending cancel says PendingCancel.
fn stale(order: &Order, answered: bool, report: &VenueReport) -> bool {
    if order.status.is_done()
        || report.cum_qty < order.cum_qty
        || (report.last_qty > Decimal::ZERO && report.cum_qty == order.cum_qty)
    {
        return true;
    }
    if report.last_qty > Decimal::ZERO {
        return false;
    }
    let current = report.downstream == order.downstream;
    // A report keeping the order's status is never done, PendingNew or New.
    let status = report.status;
    let settled =
        answered && !status.is_some_and(Status::is_done) && (!current || status == Some(Status::PendingReplace));
    let behind_new = status == Some(Status::PendingNew) && order.status != Status::PendingNew;
    let accepts_pending = order.pending.as_ref().is_some_and(|pending| pending.downstream == report.downstream);
    let behind_pending = matches!(status, Some(Status::New | Status::PartiallyFilled))
        && matches!(order.status, Status::PendingCancel | Status::PendingReplace)
        && !accepts_pending;
    settled || behind_new || behind_pending
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
    kind: CancelKind,
    /// Possibly a resend, as `Input::Cancel` says.
    redelivered: bool,
}

/// A replace's new terms, as `Input::Replace` gives them.
struct Terms {
    qty: Decimal,
    price: Option<Decimal>,
    ord_type: Option<OrdType>,
    time_in_force: Option<TimeInForce>,
}

impl Amend {
    /// An OrderCancelReject refusing it, naming the order when there is one.
    fn refuse(self, order: Option<&Order>, reason: CancelRejectReason, text: CompactString) -> Output {
        Output::CancelReject {
            client: self.client,
            order_id: order.map(|order| order.name.clone()),
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
mod tests;
