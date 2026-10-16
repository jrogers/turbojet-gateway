//! The order book: a sans-IO state machine. Each [`Input`] changes the book and pushes what to
//! send onto `out`; the FIX side turns those into typed messages for each session's version.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use turbojet::SessionId;
use turbojet::fields::{CompactString, Decimal, format_compact};

use crate::risk::{self, Exposure, Proposed, RiskReject};
use crate::router::{Router, Target};

mod amend;
mod gateway_cancel;
mod persist;
mod report;
mod view;

use amend::{Amend, Refused, Terms};
use persist::Touched;
pub use persist::{Changes, Checkpoint, ClOrdIdRow, OrderRow, Snapshot, Undelivered};
pub use view::{
    Alarm, AlarmKind, CancelState, GatewayCancelView, GatewayEvent, MAX_ORDERS_PER_PAGE, OrderDetail, OrderFilter,
    OrderView, RequestKind, RequestView, RiskView, SessionRisk,
};
pub(crate) use view::{order_view, request_view};

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

    /// The client a store kept as its session, as it shows, and CompID: all a client is.
    pub(crate) fn from_parts(session: String, comp_id: CompactString) -> Self {
        Self(Arc::new(ClientName { session, comp_id }))
    }

    /// The client's CompID, which routes match on; its sessions share it.
    pub fn comp_id(&self) -> &str {
        &self.0.comp_id
    }

    /// The session, as it shows: what an operator names the client by.
    pub fn session(&self) -> &str {
        &self.0.session
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

/// As it shows.
impl Serialize for ClientId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.session)
    }
}

/// The FIX 4.4 session from TURBOJETGATEWAY to CompID `comp_id`, with no SubIDs: tests' usual
/// client.
#[cfg(test)]
impl From<&str> for ClientId {
    fn from(comp_id: &str) -> Self {
        Self::new(&SessionId::new("FIX.4.4", "TURBOJETGATEWAY", comp_id))
    }
}

/// A client session's trading day, known by when it ends: a later day is a later end. The gateway
/// works it out from the session's schedule or `day_end`; the book only compares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(pub DateTime<Utc>);

/// A session's trading day, and when it began: its previous day's end, `None` for the first the
/// book has seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Today {
    pub day: Day,
    pub began: Option<DateTime<Utc>>,
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

/// Text(58) of the reject for a new order while the gateway is unavailable: see
/// [`Book::set_unavailable`].
pub const GATEWAY_UNAVAILABLE: &str = "gateway unavailable";

/// The book's key for an order. On the wire it's [`Order::name`], the book's prefix before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
    SellShort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    pub(crate) fn is_done(self) -> bool {
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
    /// The gateway's database is behind: see [`Book::set_unavailable`].
    GatewayUnavailable,
    /// Over one of the client's risk limits.
    Risk(RiskReject),
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
    /// The price risk counts the order at: its limit, or a Market order's reference price when
    /// it was booked (zero without one). Fixed, so its exposure doesn't move with the reference.
    pub risk_price: Decimal,
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

    /// Whether the scope covers any client of `comp_ids`: what a caller scoped to them is shown.
    pub fn concerns(&self, comp_ids: &[String]) -> bool {
        match self {
            Self::Session(client) => is_one_of(client.comp_id(), comp_ids),
            Self::CompId(comp_id) => is_one_of(comp_id, comp_ids),
            Self::All => true,
        }
    }
}

/// Whether `comp_id` is one of `comp_ids`.
pub fn is_one_of<S: AsRef<str>>(comp_id: &str, comp_ids: &[S]) -> bool {
    comp_ids.iter().any(|id| id.as_ref() == comp_id)
}

/// As its [`ScopeName`] writes it, which parses back to that name. Copies a session's name: for
/// logs and the API, not the order path.
impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&ScopeName::from(self), f)
    }
}

/// As it shows.
impl Serialize for Scope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A [`Scope`] as an operator writes it: `all`, `comp:COMPID`, or `session:SESSION` with the
/// client session as it shows. Only the syntax: a session's [`ClientId`] comes from the gateway,
/// which knows the sessions it has seen ([`Gateway::scope`](crate::edge::Gateway::scope)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeName {
    Session(String),
    CompId(CompactString),
    All,
}

impl fmt::Display for ScopeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(session) => write!(f, "session:{session}"),
            Self::CompId(comp_id) => write!(f, "comp:{comp_id}"),
            Self::All => f.write_str("all"),
        }
    }
}

impl From<&Scope> for ScopeName {
    fn from(scope: &Scope) -> Self {
        match scope {
            Scope::Session(client) => Self::Session(client.session().into()),
            Scope::CompId(comp_id) => Self::CompId(comp_id.clone()),
            Scope::All => Self::All,
        }
    }
}

impl FromStr for ScopeName {
    type Err = BadScope;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let scope = match text.split_once(':') {
            _ if text == "all" => Self::All,
            Some(("comp", comp_id)) if !comp_id.is_empty() => Self::CompId(comp_id.into()),
            Some(("session", session)) if !session.is_empty() => Self::Session(session.into()),
            _ => return Err(BadScope(text.into())),
        };
        Ok(scope)
    }
}

/// Text that isn't a [`ScopeName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadScope(pub String);

impl fmt::Display for BadScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scope {:?}: use all, comp:COMPID or session:SESSION", self.0)
    }
}

impl std::error::Error for BadScope {}

/// A replace awaiting the venue's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub downstream: CompactString,
    pub cl_ord_id: CompactString,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    pub ord_type: OrdType,
    pub time_in_force: Option<TimeInForce>,
    /// As [`Order::risk_price`], for the new terms.
    pub risk_price: Decimal,
}

/// An ExecutionReport from a venue, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueReport {
    /// The downstream ClOrdID it answers.
    pub downstream: CompactString,
    /// Passed to the client as it is, except that a report raising CumQty goes to the client as
    /// `Trade`, whatever the venue said: clients know fills by ExecType.
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
    /// `sent`: when a redelivered order was first sent, its OrigSendingTime(122) or else its
    /// SendingTime(52), if it has one. A redelivered order the book doesn't know, first sent
    /// before its session's trading day began, is rejected as a duplicate: the book forgot that
    /// day's done orders, so it can't tell whether it booked it.
    NewOrder {
        client: ClientId,
        request: OrderRequest,
        redelivered: bool,
        sent: Option<DateTime<Utc>>,
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
    /// At a start: a request the database recorded as sent to `venue` under `downstream` that the
    /// venue's session never stored, so never sent (the gateway stopped after its change committed,
    /// before the session stored it). Sent again as it was, its send time now, unless it's been
    /// answered or its order is done, when it's handled as `SendFailed`. The edge passes only
    /// requests sent since the venue's store was created or last reset: one missing from before
    /// then was lost to the reset, not the stop.
    Unsent {
        venue: VenueId,
        downstream: CompactString,
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
    /// The gateway asks the order's venue where the order stands, not to be confused with a
    /// client's `StatusRequest`, which the book answers itself. Sent under the downstream ClOrdID
    /// the venue last acknowledged ([`Order::downstream`]), not a replace still pending, which the
    /// venue may not have taken. It changes and records nothing: the venue's answer is handled as
    /// any report is, a fill the gateway missed reaching the client and one changing nothing
    /// dropped, and an answer that the venue doesn't know the order raises
    /// [`AlarmKind::UnknownAtVenue`], the order staying live, its exposure counted, until an
    /// operator resolves it ([`Input::Resolve`]). Nothing is sent for an unknown or done order.
    VenueStatusRequest {
        order: OrderId,
    },
    /// An operator's answer to [`AlarmKind::UnknownAtVenue`]: the venue doesn't know the order, so
    /// nothing will end it there, and the gateway cancels it itself, its fills kept and its
    /// exposure freed. The client hears Canceled under the order's current ClOrdID, as for a
    /// venue's unsolicited cancel: a cancel or replace of its own still outstanding isn't answered
    /// separately. Only for a live order a venue has said it doesn't know since the gateway
    /// started and hasn't reported on since ([`Book::is_unknown_at_venue`]); for any other it
    /// does nothing.
    Resolve {
        order: OrderId,
    },
    /// The venue's session logged on, or can take requests again: the gateway's cancels that
    /// couldn't be sent to it go now.
    VenueUp {
        venue: VenueId,
    },
    /// At a start: a cancel or replace sent under `downstream` (`<order name>-<n>`) that `venue`'s
    /// session stored, but the store never recorded (the gateway stopped within a commit of
    /// sending it); `replace` holds a replace's terms. Recorded as the gateway's own request, so
    /// the venue's answers apply to the order. A cancel is a cancel: the order also gets the
    /// gateway's own, waiting for `VenueUp`. A replace is pending, its terms applied if the venue
    /// accepts it, and the order keeps working: the client hears Replaced if its replace with
    /// those terms, sent again, is taken as it, else Restated. A replace waits for the one pending
    /// to be answered. One for an order the book doesn't hold (done, and dropped at a roll) is
    /// logged.
    Orphan {
        venue: VenueId,
        downstream: CompactString,
        replace: Option<OrphanedReplace>,
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
    /// Each session `client` is in trading day `day`. For a session the book has a day for, a
    /// later one rolls it over, its previous day's end being when the new one began: its done orders go, with their ClOrdIDs, which it may use again;
    /// its live orders stay (a GTC order, or one still working), their ClOrdIDs still in use, and
    /// count against its new day's [`MAX_CL_ORD_IDS_PER_CLIENT`]; what it has filled starts again
    /// at zero. Reference prices, being every client's, stay. A day no later than the session's
    /// is ignored; the session's first just records it.
    DaysStarted {
        days: Vec<(ClientId, Day)>,
    },
}

/// The terms of an orphaned replace: see [`Input::Orphan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedReplace {
    pub qty: Decimal,
    pub price: Option<Decimal>,
    pub ord_type: OrdType,
    pub time_in_force: Option<TimeInForce>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
    /// An OrderStatusRequest under the order's current `downstream`: see
    /// [`Input::VenueStatusRequest`]. `request` carries the side, symbol and account.
    StatusRequest {
        venue: VenueId,
        downstream: CompactString,
        request: OrderRequest,
        venue_order_id: Option<CompactString>,
    },
}

/// Who asked for a request we sent a venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Client,
    /// The gateway's own cancel: it spends none of the client's ClOrdIDs.
    Gateway,
}

/// A request we sent a venue, by its downstream ClOrdID: what the venue's answers map back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub order: OrderId,
    pub origin: Origin,
    /// The client's ClOrdID for the request, and its OrigClOrdID for a cancel or replace. Empty
    /// and `None` for the gateway's own cancel, whose answers go under the order's current one.
    pub cl_ord_id: CompactString,
    pub orig_cl_ord_id: Option<CompactString>,
    /// `None` for the original order.
    pub kind: Option<CancelKind>,
    /// The venue has rejected or accepted it (a cancel's Canceled, a replace's acceptance): a
    /// later reject is a resend.
    pub answered: bool,
    /// When the book last sent it: a request missing from the venue's store was lost to a stop
    /// only if it was sent since the store was created or last reset (see [`Input::Unsent`]).
    pub sent_at: DateTime<Utc>,
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
    risk: risk::State,
    /// Empty unless set by [`set_prefix`](Self::set_prefix).
    prefix: CompactString,
    orders: HashMap<OrderId, Order>,
    /// (client, ClOrdID) -> order, for every ClOrdID each client used today, and its live orders'
    /// from earlier days.
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
    /// Each client session's trading day: see [`Input::DaysStarted`].
    days: HashMap<ClientId, Today>,
    /// What changed since [`take_changes`](Self::take_changes): every change to the records
    /// above that survive a restart marks its key here.
    touched: Touched,
    /// Changes copied before a roll, for the next `take_changes`.
    flushed: Changes,
    /// New orders are rejected: see [`set_unavailable`](Self::set_unavailable).
    unavailable: bool,
    /// Blocks, unblocks and alarms since [`take_events`](Self::take_events): rare, so kept whether
    /// or not anyone listens.
    events: Vec<GatewayEvent>,
    /// Orphaned replaces (see [`Input::Orphan`]) of orders with a replace pending, by order, to be
    /// made pending once that one is answered, with the client's OrigClOrdID once its replace,
    /// sent again, is taken as it (the pending's `cl_ord_id` then the client's): see
    /// `adopt_orphaned_replace`. Not stored: a restart before then finds the orphan again in the
    /// venue's store.
    orphaned_replaces: HashMap<OrderId, (Pending, Option<CompactString>)>,
    /// Live orders a venue has said it doesn't know (see [`Book::unknown_at_venue`]), which an
    /// operator may [`Input::Resolve`]: unmarked when the venue reports on the order after all, or
    /// the order is done. An OrderCancelReject or an overfill report leaves the mark, to be safe:
    /// an operator asks again. Not stored: after a restart the operator asks the venue again.
    unknown_at_venue: HashSet<OrderId>,
    /// The time of the inputs being handled: see [`set_now`](Self::set_now).
    now: DateTime<Utc>,
    /// Every change taken in the tests, as a store would hold them.
    #[cfg(test)]
    mirror: Snapshot,
}

impl Book {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            limits: risk::Config::default(),
            risk: risk::State::default(),
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
            days: HashMap::new(),
            touched: Touched::default(),
            flushed: Changes::default(),
            unavailable: false,
            events: Vec::new(),
            orphaned_replaces: HashMap::new(),
            unknown_at_venue: HashSet::new(),
            now: DateTime::UNIX_EPOCH,
            #[cfg(test)]
            mirror: Snapshot::default(),
        }
    }

    /// Starts every ID the book issues with `prefix`; see [`Book`]. Set it before the book
    /// issues any.
    pub fn set_prefix(&mut self, prefix: CompactString) {
        debug_assert!(self.next_id == 1 && self.rejects == 0, "the prefix changed after the book issued IDs");
        self.prefix = prefix;
    }

    /// The time of the inputs handled from now on, stamped on the requests they send: the edge
    /// sets it from its clock before each batch.
    pub fn set_now(&mut self, now: DateTime<Utc>) {
        self.now = now;
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

    /// While `unavailable`, new orders are rejected ([`RejectReason::GatewayUnavailable`]): the
    /// gateway's database is too far behind to take more. Cancels, replaces and venue reports
    /// pass, as they take orders off the market or record what a venue did.
    /// Set after [`set_now`](Self::set_now): a change raises [`AlarmKind::GatewayUnavailable`] or
    /// [`AlarmKind::GatewayAvailable`].
    pub fn set_unavailable(&mut self, unavailable: bool) {
        const BEHIND: &str = "the database is too far behind: rejecting new orders";
        const CAUGHT_UP: &str = "the database has caught up: taking new orders again";
        if unavailable == self.unavailable {
            return;
        }
        self.unavailable = unavailable;
        if unavailable {
            tracing::error!("{BEHIND}");
            self.alarm(AlarmKind::GatewayUnavailable, None, BEHIND.to_owned());
        } else {
            tracing::info!("{CAUGHT_UP}");
            self.alarm(AlarmKind::GatewayAvailable, None, CAUGHT_UP.to_owned());
        }
    }

    /// Whether new orders are rejected: see [`set_unavailable`](Self::set_unavailable).
    pub fn unavailable(&self) -> bool {
        self.unavailable
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

    /// Orders not done and not being cancelled by the gateway already, per client holding any.
    pub fn working_by_client(&self) -> BTreeMap<ClientId, usize> {
        let mut working = BTreeMap::new();
        for order in self.orders.values().filter(|order| !order.status.is_done() && order.gateway_cancel.is_none()) {
            *working.entry(order.client.clone()).or_default() += 1;
        }
        working
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Whether a venue has said it doesn't know the live order, so an operator may
    /// [`Input::Resolve`] it.
    pub fn is_unknown_at_venue(&self, id: OrderId) -> bool {
        self.unknown_at_venue.contains(&id)
    }

    /// Whether the book holds an order sent to `venue`.
    pub fn holds_orders_for(&self, venue: &VenueId) -> bool {
        self.orders.values().any(|order| order.venue == *venue)
    }

    /// What was sent to a venue under `downstream`, if the book holds it.
    pub fn sent(&self, downstream: &str) -> Option<&Sent> {
        self.downstream.get(downstream)
    }

    /// The downstream ClOrdIDs of requests sent to `venue` for live orders that it hasn't
    /// answered (an original order, while it's PendingNew), with when each was sent, in the order
    /// they were sent: those [`Input::Unsent`] can send again.
    pub fn unanswered(&self, venue: &VenueId) -> Vec<(CompactString, DateTime<Utc>)> {
        let mut unanswered: Vec<(OrderId, &CompactString, DateTime<Utc>)> = self
            .downstream
            .iter()
            .filter(|(downstream, sent)| self.orders[&sent.order].venue == *venue && self.is_unanswered(downstream))
            .map(|(downstream, sent)| (sent.order, downstream, sent.sent_at))
            .collect();
        // `<name>` before `<name>-1`, and `<name>-9` before `<name>-10`.
        unanswered.sort_unstable_by_key(|&(order, downstream, _)| (order.0, downstream.len(), downstream));
        unanswered.into_iter().map(|(_, downstream, sent_at)| (downstream.clone(), sent_at)).collect()
    }

    /// Whether the request sent as `downstream` is one [`unanswered`](Self::unanswered) lists:
    /// its venue hasn't answered it, and its order is live (an original order, while it's
    /// PendingNew).
    pub fn is_unanswered(&self, downstream: &str) -> bool {
        self.downstream.get(downstream).is_some_and(|sent| {
            let order = &self.orders[&sent.order];
            !sent.answered && !order.status.is_done() && (sent.kind.is_some() || order.status == Status::PendingNew)
        })
    }

    /// Whether the request sent as `downstream` holds back a checkpoint of its venue's store (see
    /// [`edge::orphans`](crate::edge::orphans)): it's unanswered, or an orphaned replace waiting
    /// for the one pending before it, which the book doesn't count sent until it's made pending,
    /// and unanswered then.
    pub fn holds_checkpoint(&self, downstream: &str) -> bool {
        self.is_unanswered(downstream) || self.orphan_waiting(downstream)
    }

    pub fn handle(&mut self, input: Input, out: &mut Vec<Output>) {
        match input {
            Input::NewOrder { client, request, redelivered, sent } => {
                self.new_order(client, request, redelivered.then_some(sent), out)
            }
            Input::StatusRequest { client, cl_ord_id, side, symbol } => {
                self.status_request(client, cl_ord_id, side, symbol, out)
            }
            Input::VenueReport { venue, report } if report::says_unknown_at_venue(&report) => {
                self.unknown_at_venue(&venue, &report)
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
            Input::VenueStatusRequest { order } => self.venue_status_request(order, out),
            Input::Resolve { order } => self.resolve(order, out),
            Input::VenueUp { venue } => self.venue_up(&venue, out),
            Input::Unsent { venue, downstream } => self.unsent(&venue, downstream, out),
            Input::Orphan { venue, downstream, replace } => self.orphan(&venue, downstream, replace),
            Input::Block { scope } => {
                if !self.blocked.contains(&scope) {
                    self.blocked.push(scope.clone());
                    self.touched.blocked = true;
                    self.events.push(GatewayEvent::Blocked { scope });
                }
            }
            Input::Unblock { scope } => {
                if self.blocked.contains(&scope) {
                    self.blocked.retain(|blocked| *blocked != scope);
                    self.touched.blocked = true;
                    self.events.push(GatewayEvent::Unblocked { scope });
                }
            }
            Input::DaysStarted { days } => self.days_started(days),
        }
        self.promote_orphaned_replaces();
        // However the order got there: a mark is for a live order only.
        self.unknown_at_venue.retain(|id| self.orders.get(id).is_some_and(|order| !order.status.is_done()));
        #[cfg(debug_assertions)]
        self.check();
    }

    /// The book's invariants, after every input. It walks every order and request sent, so a
    /// debug build slows as the book grows, O(book) per message: don't load-test one. Release
    /// builds check only once, at restore.
    fn check(&self) {
        for (id, order) in &self.orders {
            let sent = self.downstream.get(&order.downstream).expect("an order's downstream is recorded");
            // Its original too, after a replace: when it was created.
            let original = self.downstream.get(&order.name).expect("an order's original request is recorded");
            assert_eq!(original.order, *id);
            assert_eq!(original.kind, None, "an order's name is its original request's");
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
                    // A replace only as an orphan's: see `Input::Orphan`.
                    assert!(sent.kind.is_some(), "the gateway sends no orders");
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
        for (id, (waiting, _)) in &self.orphaned_replaces {
            assert!(
                self.orders.get(id).is_some_and(|order| order.pending.is_some()),
                "{id:?}'s orphaned replace waits on none"
            );
            assert!(
                !self.downstream.contains_key(&waiting.downstream),
                "a waiting orphaned replace isn't recorded yet"
            );
        }
        for id in &self.unknown_at_venue {
            let order = self.orders.get(id).expect("an order marked unknown at its venue is held");
            assert!(!order.status.is_done(), "done order {id:?} is marked unknown at its venue");
        }
        for (client, &used) in &self.used {
            let indexed = self.index.keys().filter(|(owner, _)| owner == client).count();
            assert_eq!(used, indexed, "{client}'s ClOrdID count");
        }
        for ((client, cl_ord_id), id) in &self.index {
            assert!(self.orders.contains_key(id), "{client}'s {cl_ord_id} names no order");
            assert!(self.used.contains_key(client), "{client}'s ClOrdIDs aren't counted");
        }
        self.risk.check(self.orders.iter().filter_map(|(id, order)| Some((*id, order.exposure()?))));
    }

    /// `redelivered`: `Some` with when the order was first sent, if it says, for a redelivery.
    fn new_order(
        &mut self,
        client: ClientId,
        request: OrderRequest,
        redelivered: Option<Option<DateTime<Utc>>>,
        out: &mut Vec<Output>,
    ) {
        if let Some(&id) = self.index.get(&(client.clone(), request.cl_ord_id.clone())) {
            let order = self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders");
            // Its next ExecID, if it's answered.
            self.touched.orders.insert(id);
            // A resend of an order we booked: answer with where it stands. Side, symbol and account,
            // not the whole request, since a replace may have changed quantity and price since.
            let resent = redelivered.is_some()
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
        let began = self.days.get(&client).and_then(|today| today.began);
        if let (Some(Some(sent)), Some(began)) = (redelivered, began)
            && sent < began
        {
            let text = "ClOrdID from a previous trading day".into();
            return out.push(self.reject(client, request, RejectReason::Duplicate, text));
        }
        match self.admit(&client, &request) {
            Ok(venue) => self.book_order(client, request, venue, out),
            Err((reason, text)) => out.push(self.reject(client, request, reason, text)),
        }
    }

    /// The venue a new order goes to, or why it's rejected: the gateway is unavailable, the client
    /// is blocked or has used its ClOrdIDs, a route rejects it or none matches, or it's over a
    /// risk limit. Risk comes after routing, since self-trade prevention is per venue.
    fn admit(&self, client: &ClientId, request: &OrderRequest) -> Result<VenueId, (RejectReason, CompactString)> {
        if self.unavailable {
            return Err((RejectReason::GatewayUnavailable, GATEWAY_UNAVAILABLE.into()));
        }
        if self.is_blocked(client) {
            return Err((RejectReason::Blocked, "blocked by an operator".into()));
        }
        if self.used.get(client).is_some_and(|&used| used >= self.max_cl_ord_ids) {
            return Err((RejectReason::TooManyOrders, "too many orders today".into()));
        }
        let venue = match self.router.route(client.comp_id(), request) {
            Some(Target::Venue(venue)) => venue,
            Some(Target::Reject(text)) => return Err((RejectReason::RouteRejected, text)),
            None => return Err((RejectReason::NoRoute, "no route for this order".into())),
        };
        let proposed = Proposed {
            client,
            symbol: &request.symbol,
            venue: &venue,
            side: request.side,
            qty: request.qty,
            leaves: request.qty,
            price: limit_price(request.ord_type, request.price),
            replaces: None,
        };
        let limits = self.limits.limits(client.comp_id());
        risk::check(limits, &proposed, &self.risk)
            .map_err(|reject| (RejectReason::Risk(reject), reject.text().into()))?;
        Ok(venue)
    }

    /// Books an order `admit` let through and sends it to `venue`.
    fn book_order(&mut self, client: ClientId, request: OrderRequest, venue: VenueId, out: &mut Vec<Output>) {
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
            sent_at: self.now,
        };
        let previous = self.downstream.insert(downstream.clone(), sent);
        debug_assert!(previous.is_none(), "downstream ClOrdID {downstream} reused");
        self.touched.sent.insert(downstream.clone());
        out.push(Output::NewOrder { venue: venue.clone(), downstream: downstream.clone(), request: request.clone() });
        let risk_price = self.risk.risk_price(&request.symbol, limit_price(request.ord_type, request.price));
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
            risk_price,
            pending: None,
            exec_seq: 0,
            venue_order_id: None,
            cancel: None,
            gateway_cancel: None,
        };
        self.risk.track(id, order.exposure());
        let previous = self.orders.insert(id, order);
        debug_assert!(previous.is_none(), "order {id:?} booked twice");
        self.touched.orders.insert(id);
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
        if let Some(&id) = self.index.get(&(client.clone(), cl_ord_id.clone())) {
            let order = self.orders.get_mut(&id).expect("indexed ClOrdIDs point at booked orders");
            self.touched.orders.insert(id);
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

    /// See [`Input::VenueStatusRequest`].
    fn venue_status_request(&self, id: OrderId, out: &mut Vec<Output>) {
        let Some(order) = self.orders.get(&id) else {
            return tracing::warn!(order = id.0, "status request for an unknown order");
        };
        if order.status.is_done() {
            return tracing::debug!(order = %order.name, "status request for a done order");
        }
        tracing::info!(order = %order.name, downstream = %order.downstream, "asking the venue for an order's status");
        out.push(Output::StatusRequest {
            venue: order.venue.clone(),
            downstream: order.downstream.clone(),
            request: order.request.clone(),
            venue_order_id: order.venue_order_id.clone(),
        });
    }

    /// The scopes operators have blocked, in the order they were blocked.
    pub fn blocked(&self) -> &[Scope] {
        &self.blocked
    }

    fn is_blocked(&self, client: &ClientId) -> bool {
        self.blocked.iter().any(|scope| scope.covers(client))
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

    /// The session's trading day, if it has had one: see [`Input::DaysStarted`].
    pub fn day(&self, client: &ClientId) -> Option<Day> {
        self.days.get(client).map(|today| today.day)
    }

    /// Every session's trading day.
    pub fn days(&self) -> impl Iterator<Item = (&ClientId, Day)> {
        self.days.iter().map(|(client, today)| (client, today.day))
    }

    fn days_started(&mut self, days: Vec<(ClientId, Day)>) {
        // Written in the days they were made in, before a roll drops what they name.
        self.flush_changes();
        let mut rolling = HashSet::new();
        for (client, day) in days {
            match self.days.get_mut(&client) {
                None => {
                    self.touched.days.insert(client.clone());
                    self.days.insert(client, Today { day, began: None });
                }
                Some(today) if today.day >= day => {}
                Some(today) => {
                    *today = Today { day, began: Some(today.day.0) };
                    self.touched.days.insert(client.clone());
                    rolling.insert(client);
                }
            }
        }
        if !rolling.is_empty() {
            self.roll(&rolling);
        }
    }

    /// Starts a new trading day for the `rolling` sessions, in one pass over each of the book's
    /// maps: see [`Input::DaysStarted`].
    fn roll(&mut self, rolling: &HashSet<ClientId>) {
        let done: HashSet<OrderId> = self
            .orders
            .values()
            .filter(|order| order.status.is_done() && rolling.contains(&order.client))
            .map(|order| order.id)
            .collect();
        self.orders.retain(|id, _| !done.contains(id));
        self.downstream.retain(|_, sent| !done.contains(&sent.order));
        let mut used: HashMap<ClientId, usize> = HashMap::new();
        self.index.retain(|(owner, _), id| {
            if !rolling.contains(owner) {
                return true;
            }
            let kept = !done.contains(id);
            if kept {
                *used.entry(owner.clone()).or_default() += 1;
            }
            kept
        });
        for client in rolling {
            match used.get(client) {
                Some(&count) => self.used.insert(client.clone(), count),
                None => self.used.remove(client),
            };
            self.risk.new_day(client);
            self.touched.filled.insert(client.clone());
        }
        tracing::info!(sessions = ?rolling, dropped = done.len(), "new trading day");
    }

    fn remember(&mut self, client: &ClientId, cl_ord_id: CompactString, id: OrderId) {
        self.touched.cl_ord_ids.insert((client.clone(), cl_ord_id.clone()));
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
    /// The replace sent to the venue for `pending`: the order's request with its terms.
    pub(super) fn replace_request(&self, pending: &Pending) -> Output {
        let request = OrderRequest {
            qty: pending.qty,
            price: pending.price,
            ord_type: pending.ord_type,
            time_in_force: pending.time_in_force,
            ..self.request.clone()
        };
        Output::Replace {
            venue: self.venue.clone(),
            downstream: pending.downstream.clone(),
            orig_downstream: self.downstream.clone(),
            request,
            venue_order_id: self.venue_order_id.clone(),
        }
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
        self.risk_price = pending.risk_price;
        std::mem::replace(&mut self.downstream, pending.downstream)
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

    /// What the order counts for in risk while it's live: its leaves at its risk price, or the
    /// pending replace's, whichever is more, since the venue may accept the replace or not; it
    /// rests at the more aggressive of the two prices, for the same reason.
    fn exposure(&self) -> Option<Exposure> {
        if self.status.is_done() {
            return None;
        }
        let leaves = |qty: Decimal| (qty - self.cum_qty).max(Decimal::ZERO);
        let mut notional = risk::notional(leaves(self.request.qty), self.risk_price);
        let mut price = limit_price(self.request.ord_type, self.request.price);
        if let Some(pending) = &self.pending {
            notional = notional.max(risk::notional(leaves(pending.qty), pending.risk_price));
            // The more aggressive price, as the venue may have taken the replace: a Market one
            // crosses anything.
            price = match (price, pending.price) {
                (Some(current), Some(new)) if self.request.side == Side::Buy => Some(current.max(new)),
                (Some(current), Some(new)) => Some(current.min(new)),
                _ => None,
            };
        }
        Some(Exposure {
            client: self.client.clone(),
            symbol: self.request.symbol.clone(),
            venue: self.venue.clone(),
            side: self.request.side,
            price,
            notional,
        })
    }

    fn leaves_qty(&self) -> Decimal {
        if self.status.is_done() { Decimal::ZERO } else { self.request.qty - self.cum_qty }
    }
}

/// A Limit order's price; a Market order has none, whatever `price` says.
fn limit_price(ord_type: OrdType, price: Option<Decimal>) -> Option<Decimal> {
    if ord_type == OrdType::Market { None } else { price }
}

#[cfg(test)]
mod tests;
