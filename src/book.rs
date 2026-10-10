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
