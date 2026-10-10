//! FIX 4.2 to and from the book. Requests and venue reports parse into [`Input`]s, outputs
//! build typed messages.
//!
//! What the book doesn't model is refused at the edge with a [`MessageReject`] naming the field:
//! an OrdType other than Market or Limit, a TimeInForce other than Day, GTC, IOC or FOK, a Side
//! other than Buy, Sell or SellShort, a quantity that isn't positive, a Limit order without a
//! price and a Market order with one.
//!
//! Where FIX 4.2 differs from 4.4: an ExecutionReport says what happened in two fields,
//! ExecTransType(20) and ExecType(150). There's no ExecType Trade, a fill is PartialFill (1) or
//! Fill (2), and no ExecType OrderStatus, a status report is ExecTransType Status (3). A trade
//! bust or correction is ExecTransType Cancel (1) or Correct (2). NewOrderSingle and
//! OrderCancelReplaceRequest require HandlInst(21). The reason codes are fewer: Broker option
//! (OrdRejReason 0, CxlRejReason 2) stands in for the book's own reasons.

use turbojet::fields::{CompactString, Decimal, UtcTimestamp};
use turbojet::message::tags;
use turbojet::{FixMessage, Message, MessageReject};
use turbojet_fix42 as fix;

use super::{
    check_price, from_code_number, order_id_out, quantity, reason_code, replaced_status, required, unsupported,
};
use crate::book::{
    CancelKind, CancelRejectReason, ClientId, ClientReport, ExecType, Input, OrdType, OrderRequest, Output,
    RejectReason, Side, Status, TimeInForce, VenueId, VenueReport,
};

/// HandlInst(21) on orders and replaces sent to venues: automated execution, no broker
/// intervention. The same for every venue until one needs otherwise.
const HANDL_INST: fix::HandlInst = fix::HandlInst::AutomatedExecutionNoIntervention;

/// A client's NewOrderSingle(D), OrderCancelRequest(F), OrderCancelReplaceRequest(G) or
/// OrderStatusRequest(H) as the book's input. `redelivered` is the caller's: Turbojet's
/// `maybe_redelivered()` or [`possibly_resent`](super::possibly_resent).
///
/// # Errors
///
/// The reject to answer with: any other MsgType, a malformed message, or a value the book
/// doesn't model.
pub fn client_input(client: &ClientId, msg: &Message, redelivered: bool) -> Result<Input, MessageReject> {
    let client = client.clone();
    match msg.get(tags::MSG_TYPE) {
        Some("D") => new_order(client, msg.parse()?, redelivered),
        Some("F") => cancel(client, msg.parse()?, redelivered),
        Some("G") => replace(client, msg.parse()?, redelivered),
        Some("H") => status_request(client, msg.parse()?),
        _ => Err(MessageReject::unsupported_message_type()),
    }
}

/// The client's message for `output`: an ExecutionReport or an OrderCancelReject. `None` for
/// venue-bound outputs. `now` is TransactTime(60).
pub fn client_message(output: &Output, now: UtcTimestamp) -> Option<Message> {
    match output {
        Output::Report { report, .. } => Some(execution_report(report, now).to_message()),
        Output::CancelReject { order_id, cl_ord_id, orig_cl_ord_id, response_to, status, reason, text, .. } => {
            let response_to = match response_to {
                CancelKind::Cancel => fix::CxlRejResponseTo::OrderCancelRequest,
                // Code 2, OrderCancelReplaceRequest; the repository names it OrderCancel.
                CancelKind::Replace => fix::CxlRejResponseTo::OrderCancel,
            };
            let mut reject = fix::OrderCancelReject::new(
                order_id_out(*order_id),
                cl_ord_id.as_str(),
                orig_cl_ord_id.as_str(),
                ord_status_to_fix(*status),
                response_to,
            );
            reject.cxl_rej_reason = Some(cxl_rej_reason_to_fix(*reason));
            reject.text = Some(text.clone());
            reject.transact_time = Some(now);
            Some(reject.to_message())
        }
        Output::NewOrder { .. } | Output::Cancel { .. } | Output::Replace { .. } => None,
    }
}

/// The venue's message for `output`: a NewOrderSingle, OrderCancelRequest or
/// OrderCancelReplaceRequest under the downstream ClOrdIDs. `request.cl_ord_id` is the client's
/// and never goes to a venue. `None` for client-bound outputs. `now` is TransactTime(60).
pub fn venue_message(output: &Output, now: UtcTimestamp) -> Option<Message> {
    match output {
        Output::NewOrder { downstream, request, .. } => Some(new_order_single(downstream, request, now).to_message()),
        Output::Cancel { downstream, orig_downstream, request, venue_order_id, .. } => {
            let (symbol, side) = (request.symbol.as_str(), side_to_fix(request.side));
            let mut cancel =
                fix::OrderCancelRequest::new(orig_downstream.as_str(), downstream.as_str(), symbol, side, now);
            // Optional in FIX 4.2, so left out until the venue has named the order, not "NONE".
            cancel.order_id.clone_from(venue_order_id);
            cancel.account.clone_from(&request.account);
            cancel.order_qty = Some(request.qty);
            Some(cancel.to_message())
        }
        Output::Replace { downstream, orig_downstream, request, venue_order_id, .. } => {
            Some(replace_request(downstream, orig_downstream, request, venue_order_id.as_ref(), now).to_message())
        }
        Output::Report { .. } | Output::CancelReject { .. } => None,
    }
}

/// A venue's ExecutionReport(8) or OrderCancelReject(9) as the book's input.
///
/// `Ok(None)` for a trade bust or correction (ExecTransType Cancel or Correct): it lowers
/// CumQty, which the book would drop silently as stale, so it's logged here as an error for an
/// operator and the caller drops it. The session still takes the message: a reject wouldn't
/// make the venue send anything the book could use. ExecTypes and OrdStatuses the book doesn't
/// model, and codes outside the FIX 4.2 dictionary, are logged as warnings and passed on as
/// Restated, and as no status; an ExecTransType outside it is read as New.
///
/// Only the fields the book needs are read, from the raw message: a strict typed parse would
/// refuse the whole report, fill and all, over a venue's own code in a field the book ignores.
///
/// # Errors
///
/// The reject to answer with: any other MsgType, a field the book needs missing or malformed
/// (ClOrdID(11), which the book finds the order by, among them), or a fill without LastPx(31).
pub fn venue_input(venue: &VenueId, msg: &Message) -> Result<Option<Input>, MessageReject> {
    match msg.get(tags::MSG_TYPE) {
        Some("8") => {
            let report = venue_report(venue, msg)?;
            Ok(report.map(|report| Input::VenueReport { venue: venue.clone(), report }))
        }
        Some("9") => venue_cancel_reject(venue, msg).map(Some),
        _ => Err(MessageReject::unsupported_message_type()),
    }
}

fn new_order(client: ClientId, order: fix::NewOrderSingleRef<'_>, redelivered: bool) -> Result<Input, MessageReject> {
    let ord_type = ord_type_in(order.ord_type)?;
    let qty = quantity(order.order_qty)?;
    check_price(ord_type, order.price)?;
    let request = OrderRequest {
        cl_ord_id: order.cl_ord_id.into(),
        account: order.account.map(CompactString::from),
        symbol: order.symbol.into(),
        side: side_in(order.side)?,
        ord_type,
        qty,
        price: order.price,
        ex_destination: order.ex_destination.map(CompactString::from),
        time_in_force: order.time_in_force.map(time_in_force_in).transpose()?,
    };
    Ok(Input::NewOrder { client, request, redelivered })
}

fn cancel(client: ClientId, cancel: fix::OrderCancelRequestRef<'_>, redelivered: bool) -> Result<Input, MessageReject> {
    Ok(Input::Cancel {
        client,
        cl_ord_id: cancel.cl_ord_id.into(),
        orig_cl_ord_id: cancel.orig_cl_ord_id.into(),
        side: side_in(cancel.side)?,
        symbol: cancel.symbol.into(),
        redelivered,
    })
}

/// A FIX replace carries the order's full terms, so OrdType is always set and an absent
/// TimeInForce is Day (FIX's default), never the book's "unchanged".
fn replace(
    client: ClientId,
    replace: fix::OrderCancelReplaceRequestRef<'_>,
    redelivered: bool,
) -> Result<Input, MessageReject> {
    let ord_type = ord_type_in(replace.ord_type)?;
    let qty = quantity(replace.order_qty)?;
    check_price(ord_type, replace.price)?;
    let time_in_force = replace.time_in_force.map_or(Ok(TimeInForce::Day), time_in_force_in)?;
    Ok(Input::Replace {
        client,
        cl_ord_id: replace.cl_ord_id.into(),
        orig_cl_ord_id: replace.orig_cl_ord_id.into(),
        side: side_in(replace.side)?,
        symbol: replace.symbol.into(),
        qty,
        price: replace.price,
        ord_type: Some(ord_type),
        time_in_force: Some(time_in_force),
        redelivered,
    })
}

fn status_request(client: ClientId, request: fix::OrderStatusRequestRef<'_>) -> Result<Input, MessageReject> {
    Ok(Input::StatusRequest {
        client,
        cl_ord_id: request.cl_ord_id.into(),
        side: side_in(request.side)?,
        symbol: request.symbol.into(),
    })
}

fn side_in(side: fix::Side) -> Result<Side, MessageReject> {
    side_from_fix(side).ok_or_else(|| unsupported(tags::SIDE, "Side", side))
}

fn ord_type_in(ord_type: fix::OrdType) -> Result<OrdType, MessageReject> {
    ord_type_from_fix(ord_type).ok_or_else(|| unsupported(tags::ORD_TYPE, "OrdType", ord_type))
}

fn time_in_force_in(time_in_force: fix::TimeInForce) -> Result<TimeInForce, MessageReject> {
    time_in_force_from_fix(time_in_force).ok_or_else(|| unsupported(tags::TIME_IN_FORCE, "TimeInForce", time_in_force))
}

fn execution_report(report: &ClientReport, now: UtcTimestamp) -> fix::ExecutionReport {
    let request = &report.request;
    let (exec_trans_type, exec_type) = exec_type_to_fix(report.exec_type, report.status);
    let mut message = fix::ExecutionReport::new(
        order_id_out(report.order_id),
        report.exec_id.as_str(),
        exec_trans_type,
        exec_type,
        ord_status_to_fix(report.status),
        request.symbol.as_str(),
        side_to_fix(request.side),
        report.leaves_qty,
        report.cum_qty,
        report.avg_px,
    );
    message.cl_ord_id = Some(report.cl_ord_id.clone());
    message.orig_cl_ord_id.clone_from(&report.orig_cl_ord_id);
    message.account.clone_from(&request.account);
    message.order_qty = Some(request.qty);
    message.ord_type = Some(ord_type_to_fix(request.ord_type));
    message.price = request.price;
    message.time_in_force = request.time_in_force.map(time_in_force_to_fix);
    // FIX 4.2 requires LastShares(32) and LastPx(31) on every execution, 0 when it isn't a fill;
    // a status report isn't one.
    if exec_trans_type == fix::ExecTransType::New {
        let filled = report.last_qty > Decimal::ZERO;
        message.last_shares = Some(report.last_qty);
        message.last_px = Some(if filled { report.last_px } else { Decimal::ZERO });
    }
    message.ord_rej_reason = report.reject.map(ord_rej_reason_to_fix);
    // A status request about no order: FIX 4.2 answers Rejected, UnknownOrder.
    if report.reject.is_none() && report.order_id.is_none() && report.status == Status::Rejected {
        message.ord_rej_reason = Some(fix::OrdRejReason::UnknownOrder);
    }
    message.text.clone_from(&report.text);
    message.transact_time = Some(now);
    message
}

fn new_order_single(downstream: &CompactString, request: &OrderRequest, now: UtcTimestamp) -> fix::NewOrderSingle {
    let (symbol, side, ord_type) =
        (request.symbol.as_str(), side_to_fix(request.side), ord_type_to_fix(request.ord_type));
    let mut order = fix::NewOrderSingle::new(downstream.as_str(), HANDL_INST, symbol, side, now, ord_type);
    order.account.clone_from(&request.account);
    order.ex_destination.clone_from(&request.ex_destination);
    order.order_qty = Some(request.qty);
    order.price = request.price;
    order.time_in_force = request.time_in_force.map(time_in_force_to_fix);
    order
}

fn replace_request(
    downstream: &CompactString,
    orig_downstream: &CompactString,
    request: &OrderRequest,
    venue_order_id: Option<&CompactString>,
    now: UtcTimestamp,
) -> fix::OrderCancelReplaceRequest {
    let (symbol, side, ord_type) =
        (request.symbol.as_str(), side_to_fix(request.side), ord_type_to_fix(request.ord_type));
    let (orig, downstream) = (orig_downstream.as_str(), downstream.as_str());
    let mut replace = fix::OrderCancelReplaceRequest::new(orig, downstream, HANDL_INST, symbol, side, now, ord_type);
    replace.order_id = venue_order_id.cloned();
    replace.account.clone_from(&request.account);
    replace.ex_destination.clone_from(&request.ex_destination);
    replace.order_qty = Some(request.qty);
    replace.price = request.price;
    replace.time_in_force = request.time_in_force.map(time_in_force_to_fix);
    replace
}

fn venue_report(venue: &VenueId, msg: &Message) -> Result<Option<VenueReport>, MessageReject> {
    let downstream =
        msg.get(tags::CL_ORD_ID).ok_or(MessageReject::conditionally_required_field_missing(tags::CL_ORD_ID))?;
    let exec_id = required(msg, tags::EXEC_ID)?;
    let exec_trans_type_code = required(msg, fix::tags::EXEC_TRANS_TYPE)?;
    let exec_type_code = required(msg, tags::EXEC_TYPE)?;
    let ord_status_code = required(msg, tags::ORD_STATUS)?;
    let cum_qty: Decimal = msg.field(fix::tags::CUM_QTY)?;
    let avg_px: Decimal = msg.field(fix::tags::AVG_PX)?;
    let last_qty = msg.opt_field(fix::tags::LAST_SHARES)?.unwrap_or(Decimal::ZERO);
    let last_px = match msg.opt_field(fix::tags::LAST_PX)? {
        Some(last_px) => last_px,
        None if last_qty > Decimal::ZERO => {
            tracing::error!(%venue, downstream, exec_id, %last_qty, "fill without LastPx needs an operator; rejected");
            return Err(MessageReject::conditionally_required_field_missing(fix::tags::LAST_PX));
        }
        None => Decimal::ZERO,
    };
    let Some(exec_type) = venue_exec_type(venue, downstream, exec_trans_type_code, exec_type_code) else {
        tracing::error!(%venue, downstream, exec_id, exec_trans_type = exec_trans_type_code,
            "trade bust or correction needs an operator; dropped");
        return Ok(None);
    };
    let status = match fix::OrdStatus::from_code(ord_status_code) {
        Some(fix::OrdStatus::Replaced) => Some(replaced_status(cum_qty, msg.opt_field(fix::tags::LEAVES_QTY)?)),
        _ => venue_status(venue, downstream, ord_status_code),
    };
    Ok(Some(VenueReport {
        downstream: downstream.into(),
        exec_type,
        status,
        last_qty,
        last_px,
        cum_qty,
        avg_px,
        exec_id: exec_id.into(),
        text: msg.get(tags::TEXT).map(CompactString::from),
        // As the venue sent it: the book ignores "NONE".
        venue_order_id: msg.get(tags::ORDER_ID).map(CompactString::from),
        ord_rej_reason: reason_code(venue, downstream, msg, tags::ORD_REJ_REASON),
    }))
}

fn venue_cancel_reject(venue: &VenueId, msg: &Message) -> Result<Input, MessageReject> {
    let downstream = required(msg, tags::CL_ORD_ID)?;
    Ok(Input::VenueCancelReject {
        venue: venue.clone(),
        downstream: downstream.into(),
        status: venue_status(venue, downstream, required(msg, tags::ORD_STATUS)?),
        text: msg.get(tags::TEXT).map(CompactString::from),
        reason: reason_code(venue, downstream, msg, tags::CXL_REJ_REASON),
        venue_order_id: msg.get(tags::ORDER_ID).map(CompactString::from),
    })
}

/// The book's ExecType for a venue's ExecTransType and ExecType codes; `None` for a trade bust
/// or correction. An ExecTransType FIX 4.2 doesn't list is read as New; an ExecType the book
/// doesn't model, or FIX 4.2 doesn't list, is passed on as Restated.
fn venue_exec_type(venue: &VenueId, downstream: &str, trans_code: &str, code: &str) -> Option<ExecType> {
    let trans = fix::ExecTransType::from_code(trans_code).unwrap_or_else(|| {
        tracing::warn!(%venue, downstream, exec_trans_type = trans_code, "ExecTransType not in FIX 4.2; read as New");
        fix::ExecTransType::New
    });
    let fix_exec_type = fix::ExecType::from_code(code);
    let exec_type = exec_type_from_fix(trans, fix_exec_type.unwrap_or(fix::ExecType::Restated));
    if exec_type == Some(ExecType::Restated) && fix_exec_type != Some(fix::ExecType::Restated) {
        tracing::warn!(%venue, downstream, exec_type = code, "ExecType not modelled; passed on as Restated");
    }
    exec_type
}

/// The book's status for a venue's OrdStatus code; `None`, keeping the order's, for one it
/// doesn't model or FIX 4.2 doesn't list. DoneForDay is expected; the others are logged.
fn venue_status(venue: &VenueId, downstream: &str, code: &str) -> Option<Status> {
    let fix_status = fix::OrdStatus::from_code(code);
    let status = fix_status.and_then(ord_status_from_fix);
    if status.is_none() && fix_status != Some(fix::OrdStatus::DoneForDay) {
        tracing::warn!(%venue, downstream, ord_status = code, "OrdStatus not modelled; the order keeps its own");
    }
    status
}

// The mapping tables, one per enum pair.

fn side_from_fix(side: fix::Side) -> Option<Side> {
    match side {
        fix::Side::Buy => Some(Side::Buy),
        fix::Side::Sell => Some(Side::Sell),
        fix::Side::SellShort => Some(Side::SellShort),
        _ => None,
    }
}

fn side_to_fix(side: Side) -> fix::Side {
    match side {
        Side::Buy => fix::Side::Buy,
        Side::Sell => fix::Side::Sell,
        Side::SellShort => fix::Side::SellShort,
    }
}

fn ord_type_from_fix(ord_type: fix::OrdType) -> Option<OrdType> {
    match ord_type {
        fix::OrdType::Market => Some(OrdType::Market),
        fix::OrdType::Limit => Some(OrdType::Limit),
        _ => None,
    }
}

fn ord_type_to_fix(ord_type: OrdType) -> fix::OrdType {
    match ord_type {
        OrdType::Market => fix::OrdType::Market,
        OrdType::Limit => fix::OrdType::Limit,
    }
}

fn time_in_force_from_fix(time_in_force: fix::TimeInForce) -> Option<TimeInForce> {
    match time_in_force {
        fix::TimeInForce::Day => Some(TimeInForce::Day),
        fix::TimeInForce::GoodTillCancel => Some(TimeInForce::Gtc),
        fix::TimeInForce::ImmediateOrCancel => Some(TimeInForce::Ioc),
        fix::TimeInForce::FillOrKill => Some(TimeInForce::Fok),
        _ => None,
    }
}

fn time_in_force_to_fix(time_in_force: TimeInForce) -> fix::TimeInForce {
    match time_in_force {
        TimeInForce::Day => fix::TimeInForce::Day,
        TimeInForce::Gtc => fix::TimeInForce::GoodTillCancel,
        TimeInForce::Ioc => fix::TimeInForce::ImmediateOrCancel,
        TimeInForce::Fok => fix::TimeInForce::FillOrKill,
    }
}

fn ord_status_to_fix(status: Status) -> fix::OrdStatus {
    match status {
        Status::PendingNew => fix::OrdStatus::PendingNew,
        Status::New => fix::OrdStatus::New,
        Status::PartiallyFilled => fix::OrdStatus::PartiallyFilled,
        Status::Filled => fix::OrdStatus::Filled,
        Status::PendingCancel => fix::OrdStatus::PendingCancel,
        Status::PendingReplace => fix::OrdStatus::PendingReplace,
        Status::Canceled => fix::OrdStatus::Canceled,
        Status::Expired => fix::OrdStatus::Expired,
        Status::Rejected => fix::OrdStatus::Rejected,
    }
}

/// ExecTransType(20) and ExecType(150) for the book's ExecType and the order's status. A fill is
/// PartialFill or Fill by whether the order is now filled. A status report is ExecTransType
/// Status with the ExecType of the order's status, as FIX 4.2 has it; everything else is New.
fn exec_type_to_fix(exec_type: ExecType, status: Status) -> (fix::ExecTransType, fix::ExecType) {
    let exec_type = match exec_type {
        ExecType::PendingNew => fix::ExecType::PendingNew,
        ExecType::New => fix::ExecType::New,
        ExecType::Trade if status == Status::Filled => fix::ExecType::Fill,
        ExecType::Trade => fix::ExecType::PartialFill,
        ExecType::PendingCancel => fix::ExecType::PendingCancel,
        ExecType::Canceled => fix::ExecType::Canceled,
        ExecType::PendingReplace => fix::ExecType::PendingReplace,
        ExecType::Replaced => fix::ExecType::Replaced,
        ExecType::Rejected => fix::ExecType::Rejected,
        ExecType::Expired => fix::ExecType::Expired,
        ExecType::Restated => fix::ExecType::Restated,
        ExecType::DoneForDay => fix::ExecType::DoneForDay,
        ExecType::Status => return (fix::ExecTransType::Status, status_exec_type(status)),
    };
    (fix::ExecTransType::New, exec_type)
}

/// The ExecType a FIX 4.2 status report carries for the order's status.
fn status_exec_type(status: Status) -> fix::ExecType {
    match status {
        Status::PendingNew => fix::ExecType::PendingNew,
        Status::New => fix::ExecType::New,
        Status::PartiallyFilled => fix::ExecType::PartialFill,
        Status::Filled => fix::ExecType::Fill,
        Status::PendingCancel => fix::ExecType::PendingCancel,
        Status::PendingReplace => fix::ExecType::PendingReplace,
        Status::Canceled => fix::ExecType::Canceled,
        Status::Expired => fix::ExecType::Expired,
        Status::Rejected => fix::ExecType::Rejected,
    }
}

/// The inverse of [`ord_status_to_fix`]; `None` for the statuses the book doesn't model.
fn ord_status_from_fix(status: fix::OrdStatus) -> Option<Status> {
    match status {
        fix::OrdStatus::PendingNew => Some(Status::PendingNew),
        fix::OrdStatus::New => Some(Status::New),
        fix::OrdStatus::PartiallyFilled => Some(Status::PartiallyFilled),
        fix::OrdStatus::Filled => Some(Status::Filled),
        fix::OrdStatus::PendingCancel => Some(Status::PendingCancel),
        fix::OrdStatus::PendingReplace => Some(Status::PendingReplace),
        fix::OrdStatus::Canceled => Some(Status::Canceled),
        fix::OrdStatus::Expired => Some(Status::Expired),
        fix::OrdStatus::Rejected => Some(Status::Rejected),
        fix::OrdStatus::DoneForDay
        | fix::OrdStatus::Replaced
        | fix::OrdStatus::Stopped
        | fix::OrdStatus::Suspended
        | fix::OrdStatus::Calculated
        | fix::OrdStatus::AcceptedForBidding => None,
    }
}

/// The inverse of [`exec_type_to_fix`]. PartialFill and Fill are Trade; Stopped, Suspended and
/// Calculated are Restated: the book passes them on without acting on them. Any status report
/// is Status. `None` for a trade bust or correction.
fn exec_type_from_fix(trans: fix::ExecTransType, exec_type: fix::ExecType) -> Option<ExecType> {
    match trans {
        fix::ExecTransType::New => {}
        fix::ExecTransType::Status => return Some(ExecType::Status),
        fix::ExecTransType::Cancel | fix::ExecTransType::Correct => return None,
    }
    Some(match exec_type {
        fix::ExecType::PendingNew => ExecType::PendingNew,
        fix::ExecType::New => ExecType::New,
        fix::ExecType::PartialFill | fix::ExecType::Fill => ExecType::Trade,
        fix::ExecType::PendingCancel => ExecType::PendingCancel,
        fix::ExecType::Canceled => ExecType::Canceled,
        fix::ExecType::PendingReplace => ExecType::PendingReplace,
        fix::ExecType::Replaced => ExecType::Replaced,
        fix::ExecType::Rejected => ExecType::Rejected,
        fix::ExecType::Expired => ExecType::Expired,
        fix::ExecType::Restated | fix::ExecType::Stopped | fix::ExecType::Suspended | fix::ExecType::Calculated => {
            ExecType::Restated
        }
        fix::ExecType::DoneForDay => ExecType::DoneForDay,
    })
}

/// FIX 4.2 has a code for Duplicate (6) but none for the book's other reasons: they say Broker
/// option (0) and the report's Text says why. A venue's code passes through if FIX 4.2 has it.
fn ord_rej_reason_to_fix(reason: RejectReason) -> fix::OrdRejReason {
    match reason {
        RejectReason::Duplicate => fix::OrdRejReason::DuplicateOrder,
        RejectReason::NoRoute
        | RejectReason::RouteRejected
        | RejectReason::TooManyOrders
        | RejectReason::VenueUnavailable => fix::OrdRejReason::BrokerCredit,
        RejectReason::Venue(code) => from_code_number(code).unwrap_or(fix::OrdRejReason::BrokerCredit),
    }
}

/// FIX 4.2's CxlRejReason has codes 0 to 3 only. Duplicate, Other and a venue's code 4.2 lacks
/// become 2, Broker option (BrokerCredit in the generated enum); the Text says why.
fn cxl_rej_reason_to_fix(reason: CancelRejectReason) -> fix::CxlRejReason {
    match reason {
        CancelRejectReason::TooLate => fix::CxlRejReason::TooLateToCancel,
        CancelRejectReason::UnknownOrder => fix::CxlRejReason::UnknownOrder,
        CancelRejectReason::AlreadyPending => fix::CxlRejReason::OrderAlreadyInPendingStatus,
        CancelRejectReason::Duplicate | CancelRejectReason::Other => fix::CxlRejReason::BrokerCredit,
        CancelRejectReason::Venue(code) => from_code_number(code).unwrap_or(fix::CxlRejReason::BrokerCredit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::OrderId;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn time() -> UtcTimestamp {
        UtcTimestamp::from_timestamp(1_760_000_000, 0).unwrap()
    }

    fn client() -> ClientId {
        "FUNDA".into()
    }

    fn limit_order() -> fix::NewOrderSingle {
        let mut order = fix::NewOrderSingle::new("C1", HANDL_INST, "AAPL", fix::Side::Buy, time(), fix::OrdType::Limit);
        order.account = Some("ACC".into());
        order.ex_destination = Some("XNAS".into());
        order.order_qty = Some(dec("100"));
        order.price = Some(dec("10.5"));
        order
    }

    fn limit_replace() -> fix::OrderCancelReplaceRequest {
        let mut replace = fix::OrderCancelReplaceRequest::new(
            "C1",
            "C2",
            HANDL_INST,
            "AAPL",
            fix::Side::Buy,
            time(),
            fix::OrdType::Limit,
        );
        replace.order_qty = Some(dec("80"));
        replace.price = Some(dec("11"));
        replace
    }

    fn input(message: &impl FixMessage) -> Result<Input, MessageReject> {
        client_input(&client(), &message.to_message(), false)
    }

    fn request() -> OrderRequest {
        OrderRequest {
            cl_ord_id: "C1".into(),
            account: Some("ACC".into()),
            symbol: "AAPL".into(),
            side: Side::Sell,
            ord_type: OrdType::Limit,
            qty: dec("100"),
            price: Some(dec("10.5")),
            ex_destination: Some("XNAS".into()),
            time_in_force: Some(TimeInForce::Gtc),
        }
    }

    /// `msg` without `tag`.
    fn without(msg: &Message, tag: u32) -> Message {
        let mut out = Message::default();
        for (t, v) in msg.fields().filter(|&(t, _)| t != tag) {
            out.push(t, v);
        }
        out
    }

    /// `msg` with `tag` set to `value`, in place of any it had.
    fn with(msg: &Message, tag: u32, value: &str) -> Message {
        let mut out = without(msg, tag);
        out.push(tag, value);
        out
    }

    // Mapping tables.

    #[test]
    fn side_round_trips_and_refuses_the_rest() {
        for side in [Side::Buy, Side::Sell, Side::SellShort] {
            assert_eq!(side_from_fix(side_to_fix(side)), Some(side));
        }
        for side in [fix::Side::SellShortExempt, fix::Side::Cross, fix::Side::BuyMinus] {
            assert_eq!(side_from_fix(side), None, "{side:?}");
        }
    }

    #[test]
    fn ord_type_round_trips_and_refuses_the_rest() {
        for ord_type in [OrdType::Market, OrdType::Limit] {
            assert_eq!(ord_type_from_fix(ord_type_to_fix(ord_type)), Some(ord_type));
        }
        for ord_type in [fix::OrdType::Stop, fix::OrdType::StopLimit, fix::OrdType::Pegged] {
            assert_eq!(ord_type_from_fix(ord_type), None, "{ord_type:?}");
        }
    }

    #[test]
    fn time_in_force_round_trips_and_refuses_the_rest() {
        for tif in [TimeInForce::Day, TimeInForce::Gtc, TimeInForce::Ioc, TimeInForce::Fok] {
            assert_eq!(time_in_force_from_fix(time_in_force_to_fix(tif)), Some(tif));
        }
        for tif in [fix::TimeInForce::AtTheOpening, fix::TimeInForce::GoodTillDate, fix::TimeInForce::GoodTillCrossing]
        {
            assert_eq!(time_in_force_from_fix(tif), None, "{tif:?}");
        }
    }

    #[test]
    fn ord_status_and_exec_type_codes() {
        let statuses = [
            (Status::PendingNew, "A"),
            (Status::New, "0"),
            (Status::PartiallyFilled, "1"),
            (Status::Filled, "2"),
            (Status::PendingCancel, "6"),
            (Status::PendingReplace, "E"),
            (Status::Canceled, "4"),
            (Status::Expired, "C"),
            (Status::Rejected, "8"),
        ];
        for (status, code) in statuses {
            assert_eq!(ord_status_to_fix(status).code(), code, "{status:?}");
            // A status report's ExecType is its OrdStatus's code, ExecTransType Status.
            let (trans, exec_type) = exec_type_to_fix(ExecType::Status, status);
            assert_eq!((trans.code(), exec_type.code()), ("3", code), "{status:?}");
        }
        let exec_types = [
            (ExecType::PendingNew, Status::PendingNew, "A"),
            (ExecType::New, Status::New, "0"),
            (ExecType::Trade, Status::PartiallyFilled, "1"),
            (ExecType::Trade, Status::PendingCancel, "1"),
            (ExecType::Trade, Status::Filled, "2"),
            (ExecType::PendingCancel, Status::PendingCancel, "6"),
            (ExecType::Canceled, Status::Canceled, "4"),
            (ExecType::PendingReplace, Status::PendingReplace, "E"),
            (ExecType::Replaced, Status::New, "5"),
            (ExecType::Rejected, Status::Rejected, "8"),
            (ExecType::Expired, Status::Expired, "C"),
            (ExecType::Restated, Status::New, "D"),
            (ExecType::DoneForDay, Status::New, "3"),
        ];
        for (exec_type, status, code) in exec_types {
            let (trans, fix_exec_type) = exec_type_to_fix(exec_type, status);
            assert_eq!((trans.code(), fix_exec_type.code()), ("0", code), "{exec_type:?} {status:?}");
        }
    }

    #[test]
    fn reject_reasons_map_to_fix_42_codes() {
        let reasons = [
            (RejectReason::Duplicate, "6"),
            (RejectReason::NoRoute, "0"),
            (RejectReason::RouteRejected, "0"),
            (RejectReason::TooManyOrders, "0"),
            (RejectReason::VenueUnavailable, "0"),
            (RejectReason::Venue(3), "3"),
            (RejectReason::Venue(8), "8"),
            // FIX 4.3 added 9 onwards; a 4.2 client gets Broker option and the Text.
            (RejectReason::Venue(13), "0"),
            (RejectReason::Venue(99), "0"),
        ];
        for (reason, code) in reasons {
            assert_eq!(ord_rej_reason_to_fix(reason).code(), code, "{reason:?}");
        }
        let reasons = [
            (CancelRejectReason::TooLate, "0"),
            (CancelRejectReason::UnknownOrder, "1"),
            (CancelRejectReason::AlreadyPending, "3"),
            (CancelRejectReason::Duplicate, "2"),
            (CancelRejectReason::Other, "2"),
            (CancelRejectReason::Venue(2), "2"),
            (CancelRejectReason::Venue(6), "2"),
            (CancelRejectReason::Venue(99), "2"),
        ];
        for (reason, code) in reasons {
            assert_eq!(cxl_rej_reason_to_fix(reason).code(), code, "{reason:?}");
        }
    }

    // Client requests in.

    #[test]
    fn a_new_order_single_becomes_a_new_order() {
        let mut order = limit_order();
        order.time_in_force = Some(fix::TimeInForce::ImmediateOrCancel);
        let msg = order.to_message();
        let Input::NewOrder { client, request: parsed, redelivered } = client_input(&client(), &msg, true).unwrap()
        else {
            panic!()
        };
        assert_eq!((client.as_str(), redelivered), ("FUNDA", true));
        let expected = OrderRequest { side: Side::Buy, time_in_force: Some(TimeInForce::Ioc), ..request() };
        assert_eq!(parsed, expected);
    }

    #[test]
    fn a_new_order_without_time_in_force_keeps_it_absent() {
        let Input::NewOrder { request, .. } = input(&limit_order()).unwrap() else { panic!() };
        assert_eq!(request.time_in_force, None);
    }

    #[test]
    fn a_market_order_needs_no_price() {
        let mut order = limit_order();
        order.ord_type = fix::OrdType::Market;
        order.price = None;
        let Input::NewOrder { request, .. } = input(&order).unwrap() else { panic!() };
        assert_eq!((request.ord_type, request.price), (OrdType::Market, None));
    }

    #[test]
    fn any_handl_inst_is_taken() {
        for handl_inst in [fix::HandlInst::AutomatedExecutionInterventionOK, fix::HandlInst::ManualOrder] {
            let mut order = limit_order();
            order.handl_inst = handl_inst;
            assert!(input(&order).is_ok(), "{handl_inst:?}");
        }
    }

    /// The reject's kind and the tag it names, or the text that does for a business reject.
    fn rejected_tag(reject: &MessageReject) -> String {
        match reject {
            MessageReject::Session { ref_tag, .. } => format!("{ref_tag:?}"),
            MessageReject::Business { text, .. } => text.clone(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn new_orders_the_book_cant_take_are_rejected_on_their_field() {
        let mut cases: Vec<(Message, &str)> = Vec::new();
        let mut order = limit_order();
        order.order_qty = None;
        cases.push((order.to_message(), "Conditionally required tag 38 missing"));
        for qty in ["0", "-5"] {
            let mut order = limit_order();
            order.order_qty = Some(dec(qty));
            cases.push((order.to_message(), "Some(38)"));
        }
        let mut order = limit_order();
        order.price = None;
        cases.push((order.to_message(), "Conditionally required tag 44 missing"));
        let mut order = limit_order();
        order.ord_type = fix::OrdType::Market;
        cases.push((order.to_message(), "Some(44)"));
        let mut order = limit_order();
        order.ord_type = fix::OrdType::Stop;
        cases.push((order.to_message(), "Some(40)"));
        let mut order = limit_order();
        order.time_in_force = Some(fix::TimeInForce::GoodTillDate);
        cases.push((order.to_message(), "Some(59)"));
        let mut order = limit_order();
        order.side = fix::Side::Cross;
        cases.push((order.to_message(), "Some(54)"));
        cases.push((without(&limit_order().to_message(), tags::SYMBOL), "Some(55)"));
        cases.push((without(&limit_order().to_message(), tags::HANDL_INST), "Some(21)"));
        for (msg, expected) in cases {
            let reject = client_input(&client(), &msg, false).unwrap_err();
            assert_eq!(rejected_tag(&reject), expected, "{msg:?}: {reject}");
        }
    }

    #[test]
    fn a_cancel_request_becomes_a_cancel() {
        let cancel = fix::OrderCancelRequest::new("C1", "C2", "AAPL", fix::Side::SellShort, time());
        let expected = Input::Cancel {
            client: client(),
            cl_ord_id: "C2".into(),
            orig_cl_ord_id: "C1".into(),
            side: Side::SellShort,
            symbol: "AAPL".into(),
            redelivered: false,
        };
        assert_eq!(input(&cancel).unwrap(), expected);
        let reject = client_input(&client(), &without(&cancel.to_message(), tags::SYMBOL), false).unwrap_err();
        assert_eq!(rejected_tag(&reject), "Some(55)");
    }

    #[test]
    fn a_replace_carries_full_terms() {
        let expected = Input::Replace {
            client: client(),
            cl_ord_id: "C2".into(),
            orig_cl_ord_id: "C1".into(),
            side: Side::Buy,
            symbol: "AAPL".into(),
            qty: dec("80"),
            price: Some(dec("11")),
            ord_type: Some(OrdType::Limit),
            // Absent on the request: FIX's default, not the book's "unchanged".
            time_in_force: Some(TimeInForce::Day),
            redelivered: false,
        };
        assert_eq!(input(&limit_replace()).unwrap(), expected);
        let mut replace = limit_replace();
        replace.time_in_force = Some(fix::TimeInForce::GoodTillCancel);
        let Input::Replace { time_in_force, .. } = input(&replace).unwrap() else { panic!() };
        assert_eq!(time_in_force, Some(TimeInForce::Gtc));
    }

    #[test]
    fn replaces_the_book_cant_take_are_rejected_on_their_field() {
        let mut cases: Vec<(Message, &str)> = Vec::new();
        let mut replace = limit_replace();
        replace.order_qty = None;
        cases.push((replace.to_message(), "Conditionally required tag 38 missing"));
        let mut replace = limit_replace();
        replace.order_qty = Some(dec("0"));
        cases.push((replace.to_message(), "Some(38)"));
        let mut replace = limit_replace();
        replace.price = None;
        cases.push((replace.to_message(), "Conditionally required tag 44 missing"));
        let mut replace = limit_replace();
        replace.ord_type = fix::OrdType::Market;
        cases.push((replace.to_message(), "Some(44)"));
        let mut replace = limit_replace();
        replace.ord_type = fix::OrdType::StopLimit;
        cases.push((replace.to_message(), "Some(40)"));
        let mut replace = limit_replace();
        replace.time_in_force = Some(fix::TimeInForce::AtTheOpening);
        cases.push((replace.to_message(), "Some(59)"));
        let mut replace = limit_replace();
        replace.side = fix::Side::Undisclosed;
        cases.push((replace.to_message(), "Some(54)"));
        cases.push((without(&limit_replace().to_message(), tags::HANDL_INST), "Some(21)"));
        for (msg, expected) in cases {
            let reject = client_input(&client(), &msg, false).unwrap_err();
            assert_eq!(rejected_tag(&reject), expected, "{msg:?}: {reject}");
        }
    }

    #[test]
    fn a_status_request_becomes_a_status_request() {
        let request = fix::OrderStatusRequest::new("C1", "AAPL", fix::Side::Sell);
        let expected =
            Input::StatusRequest { client: client(), cl_ord_id: "C1".into(), side: Side::Sell, symbol: "AAPL".into() };
        assert_eq!(input(&request).unwrap(), expected);
    }

    #[test]
    fn other_message_types_are_unsupported() {
        assert_eq!(input(&venue_fill()).unwrap_err(), MessageReject::unsupported_message_type());
    }

    #[test]
    fn a_malformed_request_is_rejected() {
        let bad = without(&limit_order().to_message(), tags::SIDE);
        assert_eq!(client_input(&client(), &bad, false).unwrap_err(), MessageReject::required_tag_missing(tags::SIDE));
    }

    // Client messages out.

    fn report() -> ClientReport {
        ClientReport {
            exec_type: ExecType::Trade,
            order_id: Some(OrderId(7)),
            cl_ord_id: "C2".into(),
            orig_cl_ord_id: Some("C1".into()),
            request: request(),
            status: Status::PartiallyFilled,
            reject: None,
            last_qty: dec("40"),
            last_px: dec("10.25"),
            cum_qty: dec("40"),
            leaves_qty: dec("60"),
            avg_px: dec("10.25"),
            exec_id: "7.3".into(),
            text: Some("fill".into()),
        }
    }

    fn client_report(report: ClientReport) -> fix::ExecutionReport {
        let output = Output::Report { client: client(), report };
        client_message(&output, time()).unwrap().parse().unwrap()
    }

    #[test]
    fn a_fill_becomes_an_execution_report() {
        let er = client_report(report());
        assert_eq!((er.order_id.as_str(), er.exec_id.as_str()), ("7", "7.3"));
        assert_eq!(
            (er.exec_trans_type, er.exec_type, er.ord_status, er.side),
            (fix::ExecTransType::New, fix::ExecType::PartialFill, fix::OrdStatus::PartiallyFilled, fix::Side::Sell)
        );
        assert_eq!((er.cl_ord_id.as_deref(), er.orig_cl_ord_id.as_deref()), (Some("C2"), Some("C1")));
        assert_eq!((er.last_shares, er.last_px), (Some(dec("40")), Some(dec("10.25"))));
        assert_eq!((er.leaves_qty, er.cum_qty, er.avg_px), (dec("60"), dec("40"), dec("10.25")));
        assert_eq!((er.symbol.as_str(), er.account.as_deref()), ("AAPL", Some("ACC")));
        assert_eq!((er.order_qty, er.price), (Some(dec("100")), Some(dec("10.5"))));
        assert_eq!(
            (er.ord_type, er.time_in_force),
            (Some(fix::OrdType::Limit), Some(fix::TimeInForce::GoodTillCancel))
        );
        assert_eq!((er.ord_rej_reason, er.text.as_deref(), er.transact_time), (None, Some("fill"), Some(time())));
    }

    #[test]
    fn the_fill_that_completes_the_order_is_a_fill() {
        let filled = ClientReport { status: Status::Filled, leaves_qty: Decimal::ZERO, ..report() };
        let er = client_report(filled);
        assert_eq!(
            (er.exec_trans_type, er.exec_type, er.ord_status),
            (fix::ExecTransType::New, fix::ExecType::Fill, fix::OrdStatus::Filled)
        );
    }

    #[test]
    fn a_report_without_a_fill_says_last_shares_zero() {
        let zero = Some(Decimal::ZERO);
        let er = client_report(ClientReport { exec_type: ExecType::New, last_qty: Decimal::ZERO, ..report() });
        assert_eq!((er.last_shares, er.last_px), (zero, zero));
        // A status report isn't an execution: it carries neither.
        let er = client_report(ClientReport { exec_type: ExecType::Status, last_qty: Decimal::ZERO, ..report() });
        assert_eq!((er.last_shares, er.last_px), (None, None));
    }

    #[test]
    fn a_reject_before_booking_says_none_and_why() {
        let reject = ClientReport {
            exec_type: ExecType::Rejected,
            order_id: None,
            status: Status::Rejected,
            reject: Some(RejectReason::Duplicate),
            last_qty: Decimal::ZERO,
            text: Some("duplicate ClOrdID".into()),
            ..report()
        };
        let er = client_report(reject);
        assert_eq!((er.order_id.as_str(), er.ord_status), ("NONE", fix::OrdStatus::Rejected));
        assert_eq!(
            (er.ord_rej_reason, er.text.as_deref()),
            (Some(fix::OrdRejReason::DuplicateOrder), Some("duplicate ClOrdID"))
        );
    }

    #[test]
    fn a_status_report_is_exec_trans_type_status() {
        let status = ClientReport { exec_type: ExecType::Status, last_qty: Decimal::ZERO, ..report() };
        let er = client_report(status);
        assert_eq!(
            (er.exec_trans_type, er.exec_type, er.ord_status),
            (fix::ExecTransType::Status, fix::ExecType::PartialFill, fix::OrdStatus::PartiallyFilled)
        );
    }

    #[test]
    fn a_status_request_for_no_order_says_unknown_order() {
        let unknown = ClientReport {
            exec_type: ExecType::Status,
            order_id: None,
            status: Status::Rejected,
            last_qty: Decimal::ZERO,
            ..report()
        };
        let er = client_report(unknown);
        assert_eq!(
            (er.exec_trans_type, er.exec_type, er.ord_rej_reason),
            (fix::ExecTransType::Status, fix::ExecType::Rejected, Some(fix::OrdRejReason::UnknownOrder))
        );
    }

    #[test]
    fn a_cancel_reject_becomes_an_order_cancel_reject() {
        let output = |order_id, response_to| Output::CancelReject {
            client: client(),
            order_id,
            cl_ord_id: "C3".into(),
            orig_cl_ord_id: "C2".into(),
            response_to,
            status: Status::PendingReplace,
            reason: CancelRejectReason::AlreadyPending,
            text: "replace pending".into(),
        };
        let reject: fix::OrderCancelReject =
            client_message(&output(Some(OrderId(7)), CancelKind::Replace), time()).unwrap().parse().unwrap();
        assert_eq!(
            (reject.order_id.as_str(), reject.cl_ord_id.as_str(), reject.orig_cl_ord_id.as_str()),
            ("7", "C3", "C2")
        );
        assert_eq!(
            (reject.ord_status, reject.cxl_rej_response_to),
            (fix::OrdStatus::PendingReplace, fix::CxlRejResponseTo::OrderCancel)
        );
        assert_eq!(
            (reject.cxl_rej_reason, reject.text.as_deref()),
            (Some(fix::CxlRejReason::OrderAlreadyInPendingStatus), Some("replace pending"))
        );
        assert_eq!(reject.transact_time, Some(time()));
        let reject: fix::OrderCancelReject =
            client_message(&output(None, CancelKind::Cancel), time()).unwrap().parse().unwrap();
        assert_eq!(
            (reject.order_id.as_str(), reject.cxl_rej_response_to),
            ("NONE", fix::CxlRejResponseTo::OrderCancelRequest)
        );
    }

    #[test]
    fn venue_outputs_are_not_client_messages() {
        let output = Output::NewOrder { venue: "NASDAQ".into(), downstream: "1".into(), request: request() };
        assert!(client_message(&output, time()).is_none());
    }

    // Venue messages in.

    #[test]
    fn ord_status_round_trips_and_leaves_the_rest_to_the_order() {
        let statuses = [
            Status::PendingNew,
            Status::New,
            Status::PartiallyFilled,
            Status::Filled,
            Status::PendingCancel,
            Status::PendingReplace,
            Status::Canceled,
            Status::Expired,
            Status::Rejected,
        ];
        for status in statuses {
            assert_eq!(ord_status_from_fix(ord_status_to_fix(status)), Some(status));
        }
        let unmodelled = [
            fix::OrdStatus::DoneForDay,
            fix::OrdStatus::Replaced,
            fix::OrdStatus::Stopped,
            fix::OrdStatus::Suspended,
            fix::OrdStatus::Calculated,
            fix::OrdStatus::AcceptedForBidding,
        ];
        for status in unmodelled {
            assert_eq!(ord_status_from_fix(status), None, "{status:?}");
        }
    }

    #[test]
    fn exec_type_round_trips_and_unmodelled_ones_are_restated() {
        let exec_types = [
            (ExecType::PendingNew, Status::PendingNew),
            (ExecType::New, Status::New),
            (ExecType::Trade, Status::PartiallyFilled),
            (ExecType::Trade, Status::Filled),
            (ExecType::PendingCancel, Status::PendingCancel),
            (ExecType::Canceled, Status::Canceled),
            (ExecType::PendingReplace, Status::PendingReplace),
            (ExecType::Replaced, Status::New),
            (ExecType::Rejected, Status::Rejected),
            (ExecType::Expired, Status::Expired),
            (ExecType::Restated, Status::New),
            (ExecType::DoneForDay, Status::New),
            (ExecType::Status, Status::New),
            (ExecType::Status, Status::Filled),
        ];
        for (exec_type, status) in exec_types {
            let (trans, fix_exec_type) = exec_type_to_fix(exec_type, status);
            assert_eq!(exec_type_from_fix(trans, fix_exec_type), Some(exec_type), "{exec_type:?} {status:?}");
        }
        for exec_type in [fix::ExecType::Stopped, fix::ExecType::Suspended, fix::ExecType::Calculated] {
            assert_eq!(exec_type_from_fix(fix::ExecTransType::New, exec_type), Some(ExecType::Restated));
        }
        for trans in [fix::ExecTransType::Cancel, fix::ExecTransType::Correct] {
            assert_eq!(exec_type_from_fix(trans, fix::ExecType::Fill), None, "{trans:?}");
        }
    }

    fn venue() -> VenueId {
        "NASDAQ".into()
    }

    fn venue_fill() -> fix::ExecutionReport {
        let mut report = fix::ExecutionReport::new(
            "V-9",
            "X1",
            fix::ExecTransType::New,
            fix::ExecType::PartialFill,
            fix::OrdStatus::PartiallyFilled,
            "AAPL",
            fix::Side::Buy,
            dec("60"),
            dec("40"),
            dec("10.25"),
        );
        report.cl_ord_id = Some("1".into());
        report.last_shares = Some(dec("40"));
        report.last_px = Some(dec("10.25"));
        report.text = Some("fill".into());
        report
    }

    fn venue_report_of(report: &fix::ExecutionReport) -> Option<VenueReport> {
        match venue_input(&venue(), &report.to_message()).unwrap() {
            Some(Input::VenueReport { venue, report }) => {
                assert_eq!(venue, "NASDAQ");
                Some(report)
            }
            None => None,
            Some(other) => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_venue_fill_becomes_a_venue_report() {
        let expected = VenueReport {
            downstream: "1".into(),
            exec_type: ExecType::Trade,
            status: Some(Status::PartiallyFilled),
            last_qty: dec("40"),
            last_px: dec("10.25"),
            cum_qty: dec("40"),
            avg_px: dec("10.25"),
            exec_id: "X1".into(),
            text: Some("fill".into()),
            venue_order_id: Some("V-9".into()),
            ord_rej_reason: None,
        };
        assert_eq!(venue_report_of(&venue_fill()), Some(expected));
        let mut fill = venue_fill();
        (fill.exec_type, fill.ord_status, fill.leaves_qty) =
            (fix::ExecType::Fill, fix::OrdStatus::Filled, Decimal::ZERO);
        let report = venue_report_of(&fill).unwrap();
        assert_eq!((report.exec_type, report.status), (ExecType::Trade, Some(Status::Filled)));
    }

    #[test]
    fn a_venue_status_report_is_status() {
        let mut report = venue_fill();
        (report.exec_trans_type, report.last_shares, report.last_px) = (fix::ExecTransType::Status, None, None);
        let report = venue_report_of(&report).unwrap();
        assert_eq!((report.exec_type, report.status), (ExecType::Status, Some(Status::PartiallyFilled)));
    }

    #[test]
    fn a_venue_reject_keeps_its_reason_code_and_order_id_none() {
        let mut report = venue_fill();
        (report.order_id, report.exec_type, report.ord_status) =
            ("NONE".into(), fix::ExecType::Rejected, fix::OrdStatus::Rejected);
        (report.last_shares, report.last_px, report.cum_qty) = (None, None, Decimal::ZERO);
        report.ord_rej_reason = Some(fix::OrdRejReason::UnknownSymbol);
        let report = venue_report_of(&report).unwrap();
        assert_eq!((report.exec_type, report.status), (ExecType::Rejected, Some(Status::Rejected)));
        assert_eq!((report.last_qty, report.last_px), (Decimal::ZERO, Decimal::ZERO));
        assert_eq!((report.ord_rej_reason, report.venue_order_id.as_deref()), (Some(1), Some("NONE")));
    }

    #[test]
    fn unmodelled_venue_reports_keep_the_orders_status() {
        let cases = [
            (fix::ExecType::DoneForDay, fix::OrdStatus::DoneForDay, ExecType::DoneForDay),
            (fix::ExecType::Suspended, fix::OrdStatus::Suspended, ExecType::Restated),
            (fix::ExecType::Calculated, fix::OrdStatus::Calculated, ExecType::Restated),
            (fix::ExecType::Stopped, fix::OrdStatus::Stopped, ExecType::Restated),
        ];
        for (fix_exec_type, ord_status, exec_type) in cases {
            let mut report = venue_fill();
            (report.exec_type, report.ord_status) = (fix_exec_type, ord_status);
            let report = venue_report_of(&report).unwrap();
            assert_eq!((report.exec_type, report.status), (exec_type, None), "{fix_exec_type:?}");
        }
    }

    #[test]
    fn a_replaced_ord_status_is_the_order_working() {
        for (cum_qty, status) in [("0", Status::New), ("40", Status::PartiallyFilled)] {
            let mut report = venue_fill();
            (report.exec_type, report.ord_status, report.cum_qty) =
                (fix::ExecType::Replaced, fix::OrdStatus::Replaced, dec(cum_qty));
            let report = venue_report_of(&report).unwrap();
            assert_eq!((report.exec_type, report.status), (ExecType::Replaced, Some(status)), "{cum_qty}");
        }
    }

    #[test]
    fn a_replaced_ord_status_with_nothing_left_is_filled() {
        let mut report = venue_fill();
        (report.exec_type, report.ord_status, report.cum_qty, report.leaves_qty) =
            (fix::ExecType::Replaced, fix::OrdStatus::Replaced, dec("80"), Decimal::ZERO);
        assert_eq!(venue_report_of(&report).unwrap().status, Some(Status::Filled));
    }

    #[test]
    fn trade_busts_and_corrections_are_dropped() {
        for trans in [fix::ExecTransType::Cancel, fix::ExecTransType::Correct] {
            let mut report = venue_fill();
            report.exec_trans_type = trans;
            assert_eq!(venue_report_of(&report), None, "{trans:?}");
        }
    }

    #[test]
    fn a_venue_report_without_a_cl_ord_id_is_rejected() {
        let mut report = venue_fill();
        report.cl_ord_id = None;
        let reject = venue_input(&venue(), &report.to_message()).unwrap_err();
        assert_eq!(reject, MessageReject::conditionally_required_field_missing(tags::CL_ORD_ID));
    }

    fn venue_cancel_reject_message() -> fix::OrderCancelReject {
        let mut reject = fix::OrderCancelReject::new(
            "V-9",
            "1-1",
            "1",
            fix::OrdStatus::New,
            fix::CxlRejResponseTo::OrderCancelRequest,
        );
        reject.cxl_rej_reason = Some(fix::CxlRejReason::TooLateToCancel);
        reject.text = Some("too late".into());
        reject
    }

    #[test]
    fn a_venue_cancel_reject_becomes_a_venue_cancel_reject() {
        let mut reject = venue_cancel_reject_message();
        let expected = Input::VenueCancelReject {
            venue: venue(),
            downstream: "1-1".into(),
            status: Some(Status::New),
            text: Some("too late".into()),
            reason: Some(0),
            venue_order_id: Some("V-9".into()),
        };
        assert_eq!(venue_input(&venue(), &reject.to_message()).unwrap(), Some(expected));
        reject.ord_status = fix::OrdStatus::DoneForDay;
        let Some(Input::VenueCancelReject { status, .. }) = venue_input(&venue(), &reject.to_message()).unwrap() else {
            panic!()
        };
        assert_eq!(status, None);
    }

    #[test]
    fn other_venue_message_types_are_unsupported() {
        let msg = limit_order().to_message();
        assert_eq!(venue_input(&venue(), &msg).unwrap_err(), MessageReject::unsupported_message_type());
    }

    fn venue_report_from(msg: &Message) -> VenueReport {
        let Some(Input::VenueReport { report, .. }) = venue_input(&venue(), msg).unwrap() else { panic!("{msg:?}") };
        report
    }

    #[test]
    fn codes_outside_the_fix_42_dictionary_dont_lose_a_report() {
        let fill = venue_fill().to_message();
        // ExecRestatementReason(378), LastCapacity(29), Rule80A(47): not read, so any code passes.
        for (tag, value) in [(378, "123"), (29, "9"), (47, "Q")] {
            assert_eq!(venue_report_from(&with(&fill, tag, value)).last_qty, dec("40"), "{tag}={value}");
        }
        let report = venue_report_from(&with(&fill, fix::tags::ORD_REJ_REASON, "100"));
        assert_eq!(report.ord_rej_reason, Some(100));
        let report = venue_report_from(&with(&fill, fix::tags::ORD_REJ_REASON, "X"));
        assert_eq!(report.ord_rej_reason, None);
        // FIX 4.4's Trade is outside FIX 4.2's ExecType.
        let report = venue_report_from(&with(&fill, tags::EXEC_TYPE, "F"));
        assert_eq!((report.exec_type, report.last_qty), (ExecType::Restated, dec("40")));
        let report = venue_report_from(&with(&fill, tags::EXEC_TRANS_TYPE, "Z"));
        assert_eq!((report.exec_type, report.last_qty), (ExecType::Trade, dec("40")));
        let report = venue_report_from(&with(&fill, tags::ORD_STATUS, "Z"));
        assert_eq!(report.status, None);

        let reject = venue_cancel_reject_message().to_message();
        let msg = with(&reject, fix::tags::CXL_REJ_REASON, "100");
        let Some(Input::VenueCancelReject { reason, status, .. }) = venue_input(&venue(), &msg).unwrap() else {
            panic!()
        };
        assert_eq!((reason, status), (Some(100), Some(Status::New)));
        let msg = with(&reject, tags::CXL_REJ_RESPONSE_TO, "7");
        assert!(venue_input(&venue(), &msg).unwrap().is_some());
    }

    #[test]
    fn venue_reports_without_what_the_book_needs_are_rejected() {
        let fill = venue_fill().to_message();
        let needed = [
            tags::EXEC_ID,
            tags::EXEC_TRANS_TYPE,
            tags::ORD_STATUS,
            tags::EXEC_TYPE,
            fix::tags::CUM_QTY,
            fix::tags::AVG_PX,
        ];
        for tag in needed {
            let msg = without(&fill, tag);
            assert_eq!(venue_input(&venue(), &msg).unwrap_err(), MessageReject::required_tag_missing(tag), "{tag}");
        }
        let bad_qty = with(&fill, fix::tags::CUM_QTY, "lots");
        assert!(matches!(venue_input(&venue(), &bad_qty), Err(MessageReject::Session { ref_tag: Some(14), .. })));
    }

    #[test]
    fn a_fill_without_a_price_is_refused() {
        let mut fill = venue_fill();
        fill.last_px = None;
        let reject = venue_input(&venue(), &fill.to_message()).unwrap_err();
        assert_eq!(reject, MessageReject::conditionally_required_field_missing(fix::tags::LAST_PX));
    }

    // Venue messages out.

    /// The client's ClOrdID, to check it never reaches a venue.
    const CLIENT_CL_ORD_ID: &str = "CLIENT-SECRET-ClOrdID";

    fn venue_request() -> OrderRequest {
        OrderRequest { cl_ord_id: CLIENT_CL_ORD_ID.into(), ..request() }
    }

    fn venue_outputs() -> [Output; 3] {
        [
            Output::NewOrder { venue: venue(), downstream: "7".into(), request: venue_request() },
            Output::Cancel {
                venue: venue(),
                downstream: "7-2".into(),
                orig_downstream: "7-1".into(),
                request: venue_request(),
                venue_order_id: Some("V-9".into()),
            },
            Output::Replace {
                venue: venue(),
                downstream: "7-1".into(),
                orig_downstream: "7".into(),
                request: venue_request(),
                venue_order_id: Some("V-9".into()),
            },
        ]
    }

    #[test]
    fn the_clients_cl_ord_id_never_reaches_a_venue() {
        for output in venue_outputs() {
            let msg = venue_message(&output, time()).unwrap();
            for (tag, value) in msg.fields() {
                assert!(!value.contains(CLIENT_CL_ORD_ID), "{tag}={value} in {output:?}");
            }
        }
    }

    #[test]
    fn a_new_order_goes_to_the_venue_under_its_downstream_cl_ord_id() {
        let [output, ..] = venue_outputs();
        let order: fix::NewOrderSingle = venue_message(&output, time()).unwrap().parse().unwrap();
        assert_eq!((order.cl_ord_id.as_str(), order.side, order.transact_time), ("7", fix::Side::Sell, time()));
        assert_eq!((order.symbol.as_str(), order.account.as_deref()), ("AAPL", Some("ACC")));
        assert_eq!((order.ex_destination.as_deref(), order.handl_inst), (Some("XNAS"), HANDL_INST));
        assert_eq!(
            (order.ord_type, order.order_qty, order.price),
            (fix::OrdType::Limit, Some(dec("100")), Some(dec("10.5")))
        );
        assert_eq!(order.time_in_force, Some(fix::TimeInForce::GoodTillCancel));
    }

    #[test]
    fn a_cancel_names_the_downstream_order_and_the_venues_order_id() {
        let [_, mut output, _] = venue_outputs();
        let cancel: fix::OrderCancelRequest = venue_message(&output, time()).unwrap().parse().unwrap();
        assert_eq!((cancel.cl_ord_id.as_str(), cancel.orig_cl_ord_id.as_str()), ("7-2", "7-1"));
        assert_eq!(
            (cancel.order_id.as_deref(), cancel.side, cancel.transact_time),
            (Some("V-9"), fix::Side::Sell, time())
        );
        assert_eq!((cancel.symbol.as_str(), cancel.order_qty), ("AAPL", Some(dec("100"))));
        let Output::Cancel { venue_order_id, .. } = &mut output else { unreachable!() };
        *venue_order_id = None;
        let msg = venue_message(&output, time()).unwrap();
        assert_eq!(msg.get(tags::ORDER_ID), None);
    }

    #[test]
    fn a_replace_carries_the_new_terms() {
        let [_, _, output] = venue_outputs();
        let replace: fix::OrderCancelReplaceRequest = venue_message(&output, time()).unwrap().parse().unwrap();
        assert_eq!((replace.cl_ord_id.as_str(), replace.orig_cl_ord_id.as_str()), ("7-1", "7"));
        assert_eq!(
            (replace.order_id.as_deref(), replace.transact_time, replace.handl_inst),
            (Some("V-9"), time(), HANDL_INST)
        );
        assert_eq!(
            (replace.ord_type, replace.order_qty, replace.price),
            (fix::OrdType::Limit, Some(dec("100")), Some(dec("10.5")))
        );
        assert_eq!((replace.symbol.as_str(), replace.side), ("AAPL", fix::Side::Sell));
        assert_eq!(replace.time_in_force, Some(fix::TimeInForce::GoodTillCancel));
    }

    #[test]
    fn client_outputs_are_not_venue_messages() {
        let output = Output::Report { client: client(), report: report() };
        assert!(venue_message(&output, time()).is_none());
    }
}
