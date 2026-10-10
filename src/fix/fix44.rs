//! FIX 4.4 to and from the book. Requests parse into [`Input`]s, outputs build typed messages.
//!
//! What the book doesn't model is refused at the edge with a [`MessageReject`] naming the field:
//! an OrdType other than Market or Limit, a TimeInForce other than Day, GTC, IOC or FOK, a Side
//! other than Buy, Sell or SellShort, a quantity that isn't positive, a Limit order without a
//! price and a Market order with one.

use turbojet::fields::{CompactString, Decimal, FixEnum, UtcTimestamp, format_compact};
use turbojet::message::tags;
use turbojet::{FixMessage, Message, MessageReject};
use turbojet_fix44 as fix;

use crate::book::{
    CancelKind, CancelRejectReason, ClientId, ClientReport, ExecType, Input, OrdType, OrderId, OrderRequest, Output,
    RejectReason, Side, Status, TimeInForce,
};

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

fn new_order(client: ClientId, order: fix::NewOrderSingleRef<'_>, redelivered: bool) -> Result<Input, MessageReject> {
    let ord_type = ord_type_in(order.ord_type)?;
    let qty = quantity(order.order_qty)?;
    check_price(ord_type, order.price)?;
    let request = OrderRequest {
        cl_ord_id: order.cl_ord_id.into(),
        account: order.account.map(CompactString::from),
        symbol: symbol(order.symbol)?,
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
        symbol: symbol(cancel.symbol)?,
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
        symbol: symbol(replace.symbol)?,
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
        symbol: symbol(request.symbol)?,
    })
}

/// Symbol(55): optional in FIX 4.4's Instrument block, but the book routes and checks on it.
fn symbol(symbol: Option<&str>) -> Result<CompactString, MessageReject> {
    symbol.map(CompactString::from).ok_or(MessageReject::required_tag_missing(tags::SYMBOL))
}

/// OrderQty(38): optional in FIX 4.4 (CashOrderQty may stand in), but the book needs it.
fn quantity(qty: Option<Decimal>) -> Result<Decimal, MessageReject> {
    let qty = qty.ok_or(MessageReject::conditionally_required_field_missing(tags::ORDER_QTY))?;
    if qty <= Decimal::ZERO {
        return Err(MessageReject::value_incorrect(tags::ORDER_QTY, "OrderQty must be positive"));
    }
    Ok(qty)
}

/// A Limit order needs Price(44) and a Market order mustn't have one.
fn check_price(ord_type: OrdType, price: Option<Decimal>) -> Result<(), MessageReject> {
    match (ord_type, price) {
        (OrdType::Limit, None) => Err(MessageReject::conditionally_required_field_missing(tags::PRICE)),
        (OrdType::Market, Some(_)) => Err(MessageReject::value_incorrect(tags::PRICE, "A market order takes no Price")),
        _ => Ok(()),
    }
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

fn unsupported(tag: u32, name: &str, value: impl FixEnum + std::fmt::Debug) -> MessageReject {
    MessageReject::value_incorrect(tag, format!("{name} {value:?} ({}) is not supported", value.code()))
}

fn execution_report(report: &ClientReport, now: UtcTimestamp) -> fix::ExecutionReport {
    let mut message = fix::ExecutionReport::new(
        order_id_out(report.order_id),
        report.exec_id.as_str(),
        exec_type_to_fix(report.exec_type),
        ord_status_to_fix(report.status),
        side_to_fix(report.request.side),
        report.leaves_qty,
        report.cum_qty,
        report.avg_px,
    );
    let request = &report.request;
    message.cl_ord_id = Some(report.cl_ord_id.clone());
    message.orig_cl_ord_id.clone_from(&report.orig_cl_ord_id);
    message.account.clone_from(&request.account);
    message.symbol = Some(request.symbol.clone());
    message.order_qty = Some(request.qty);
    message.ord_type = Some(ord_type_to_fix(request.ord_type));
    message.price = request.price;
    message.time_in_force = request.time_in_force.map(time_in_force_to_fix);
    if report.last_qty > Decimal::ZERO {
        message.last_qty = Some(report.last_qty);
        message.last_px = Some(report.last_px);
    }
    message.ord_rej_reason = report.reject.map(ord_rej_reason_to_fix);
    // A status request about no order: FIX 4.4 answers Rejected, UnknownOrder.
    if report.reject.is_none() && report.order_id.is_none() && report.status == Status::Rejected {
        message.ord_rej_reason = Some(fix::OrdRejReason::UnknownOrder);
    }
    message.text.clone_from(&report.text);
    message.transact_time = Some(now);
    message
}

/// OrderID(37) is required on ExecutionReport and OrderCancelReject; "NONE" for no order.
fn order_id_out(order_id: Option<OrderId>) -> CompactString {
    order_id.map_or_else(|| "NONE".into(), |id| format_compact!("{}", id.0))
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

fn exec_type_to_fix(exec_type: ExecType) -> fix::ExecType {
    match exec_type {
        ExecType::PendingNew => fix::ExecType::PendingNew,
        ExecType::New => fix::ExecType::New,
        ExecType::Trade => fix::ExecType::Trade,
        ExecType::PendingCancel => fix::ExecType::PendingCancel,
        ExecType::Canceled => fix::ExecType::Canceled,
        ExecType::PendingReplace => fix::ExecType::PendingReplace,
        ExecType::Replaced => fix::ExecType::Replaced,
        ExecType::Rejected => fix::ExecType::Rejected,
        ExecType::Expired => fix::ExecType::Expired,
        ExecType::Restated => fix::ExecType::Restated,
        ExecType::DoneForDay => fix::ExecType::DoneForDay,
        ExecType::Status => fix::ExecType::OrderStatus,
    }
}

/// The book's reasons have no FIX 4.4 code of their own but Duplicate; the rest say Other (99)
/// and the report's Text says why. A venue's code passes through if FIX 4.4 has it.
fn ord_rej_reason_to_fix(reason: RejectReason) -> fix::OrdRejReason {
    match reason {
        RejectReason::Duplicate => fix::OrdRejReason::DuplicateOrder,
        RejectReason::NoRoute
        | RejectReason::RouteRejected
        | RejectReason::TooManyOrders
        | RejectReason::VenueUnavailable => fix::OrdRejReason::Other,
        RejectReason::Venue(code) => from_code_number(code).unwrap_or(fix::OrdRejReason::Other),
    }
}

fn cxl_rej_reason_to_fix(reason: CancelRejectReason) -> fix::CxlRejReason {
    match reason {
        CancelRejectReason::TooLate => fix::CxlRejReason::TooLateToCancel,
        CancelRejectReason::UnknownOrder => fix::CxlRejReason::UnknownOrder,
        CancelRejectReason::AlreadyPending => fix::CxlRejReason::OrderAlreadyInPendingStatus,
        CancelRejectReason::Duplicate => fix::CxlRejReason::DuplicateClOrdID,
        CancelRejectReason::Other => fix::CxlRejReason::Other,
        CancelRejectReason::Venue(code) => from_code_number(code).unwrap_or(fix::CxlRejReason::Other),
    }
}

/// The enum value for a numeric FIX code, if the enum has it.
fn from_code_number<E: FixEnum>(code: u32) -> Option<E> {
    E::from_code(&format_compact!("{code}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut order = fix::NewOrderSingle::new("C1", fix::Side::Buy, time(), fix::OrdType::Limit);
        order.symbol = Some("AAPL".into());
        order.account = Some("ACC".into());
        order.ex_destination = Some("XNAS".into());
        order.order_qty = Some(dec("100"));
        order.price = Some(dec("10.5"));
        order
    }

    fn limit_replace() -> fix::OrderCancelReplaceRequest {
        let mut replace = fix::OrderCancelReplaceRequest::new("C1", "C2", fix::Side::Buy, time(), fix::OrdType::Limit);
        replace.symbol = Some("AAPL".into());
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
        for tif in [fix::TimeInForce::AtTheOpening, fix::TimeInForce::GoodTillDate, fix::TimeInForce::AtTheClose] {
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
        }
        let exec_types = [
            (ExecType::PendingNew, "A"),
            (ExecType::New, "0"),
            (ExecType::Trade, "F"),
            (ExecType::PendingCancel, "6"),
            (ExecType::Canceled, "4"),
            (ExecType::PendingReplace, "E"),
            (ExecType::Replaced, "5"),
            (ExecType::Rejected, "8"),
            (ExecType::Expired, "C"),
            (ExecType::Restated, "D"),
            (ExecType::DoneForDay, "3"),
            (ExecType::Status, "I"),
        ];
        for (exec_type, code) in exec_types {
            assert_eq!(exec_type_to_fix(exec_type).code(), code, "{exec_type:?}");
        }
    }

    #[test]
    fn reject_reasons_map_to_fix_44_codes() {
        let reasons = [
            (RejectReason::Duplicate, "6"),
            (RejectReason::NoRoute, "99"),
            (RejectReason::RouteRejected, "99"),
            (RejectReason::TooManyOrders, "99"),
            (RejectReason::VenueUnavailable, "99"),
            (RejectReason::Venue(3), "3"),
            (RejectReason::Venue(15), "15"),
            // FIX 5.0 added 16 onwards; a 4.4 client gets Other and the Text.
            (RejectReason::Venue(18), "99"),
        ];
        for (reason, code) in reasons {
            assert_eq!(ord_rej_reason_to_fix(reason).code(), code, "{reason:?}");
        }
        let reasons = [
            (CancelRejectReason::TooLate, "0"),
            (CancelRejectReason::UnknownOrder, "1"),
            (CancelRejectReason::AlreadyPending, "3"),
            (CancelRejectReason::Duplicate, "6"),
            (CancelRejectReason::Other, "99"),
            (CancelRejectReason::Venue(2), "2"),
            (CancelRejectReason::Venue(7), "99"),
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
        let mut cases: Vec<(fix::NewOrderSingle, &str)> = Vec::new();
        let mut order = limit_order();
        order.order_qty = None;
        cases.push((order, "Conditionally required tag 38 missing"));
        for qty in ["0", "-5"] {
            let mut order = limit_order();
            order.order_qty = Some(dec(qty));
            cases.push((order, "Some(38)"));
        }
        let mut order = limit_order();
        order.price = None;
        cases.push((order, "Conditionally required tag 44 missing"));
        let mut order = limit_order();
        order.ord_type = fix::OrdType::Market;
        cases.push((order, "Some(44)"));
        let mut order = limit_order();
        order.ord_type = fix::OrdType::Stop;
        cases.push((order, "Some(40)"));
        let mut order = limit_order();
        order.time_in_force = Some(fix::TimeInForce::GoodTillDate);
        cases.push((order, "Some(59)"));
        let mut order = limit_order();
        order.side = fix::Side::Cross;
        cases.push((order, "Some(54)"));
        let mut order = limit_order();
        order.symbol = None;
        cases.push((order, "Some(55)"));
        for (order, expected) in cases {
            let reject = input(&order).unwrap_err();
            assert_eq!(rejected_tag(&reject), expected, "{order:?}: {reject}");
        }
    }

    #[test]
    fn a_cancel_request_becomes_a_cancel() {
        let mut cancel = fix::OrderCancelRequest::new("C1", "C2", fix::Side::SellShort, time());
        cancel.symbol = Some("AAPL".into());
        let expected = Input::Cancel {
            client: client(),
            cl_ord_id: "C2".into(),
            orig_cl_ord_id: "C1".into(),
            side: Side::SellShort,
            symbol: "AAPL".into(),
            redelivered: false,
        };
        assert_eq!(input(&cancel).unwrap(), expected);
        cancel.symbol = None;
        assert_eq!(rejected_tag(&input(&cancel).unwrap_err()), "Some(55)");
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
        let mut cases: Vec<(fix::OrderCancelReplaceRequest, &str)> = Vec::new();
        let mut replace = limit_replace();
        replace.order_qty = None;
        cases.push((replace, "Conditionally required tag 38 missing"));
        let mut replace = limit_replace();
        replace.order_qty = Some(dec("0"));
        cases.push((replace, "Some(38)"));
        let mut replace = limit_replace();
        replace.price = None;
        cases.push((replace, "Conditionally required tag 44 missing"));
        let mut replace = limit_replace();
        replace.ord_type = fix::OrdType::Market;
        cases.push((replace, "Some(44)"));
        let mut replace = limit_replace();
        replace.ord_type = fix::OrdType::StopLimit;
        cases.push((replace, "Some(40)"));
        let mut replace = limit_replace();
        replace.time_in_force = Some(fix::TimeInForce::AtTheClose);
        cases.push((replace, "Some(59)"));
        let mut replace = limit_replace();
        replace.side = fix::Side::Undisclosed;
        cases.push((replace, "Some(54)"));
        for (replace, expected) in cases {
            let reject = input(&replace).unwrap_err();
            assert_eq!(rejected_tag(&reject), expected, "{replace:?}: {reject}");
        }
    }

    #[test]
    fn a_status_request_becomes_a_status_request() {
        let mut request = fix::OrderStatusRequest::new("C1", fix::Side::Sell);
        request.symbol = Some("AAPL".into());
        let expected =
            Input::StatusRequest { client: client(), cl_ord_id: "C1".into(), side: Side::Sell, symbol: "AAPL".into() };
        assert_eq!(input(&request).unwrap(), expected);
    }

    #[test]
    fn other_message_types_are_unsupported() {
        let report = fix::ExecutionReport::new(
            "1",
            "E",
            fix::ExecType::New,
            fix::OrdStatus::New,
            fix::Side::Buy,
            0.into(),
            0.into(),
            0.into(),
        );
        assert_eq!(input(&report).unwrap_err(), MessageReject::unsupported_message_type());
    }

    #[test]
    fn a_malformed_request_is_rejected() {
        let msg = limit_order().to_message();
        let mut bad = Message::default();
        for (tag, value) in msg.fields().filter(|&(tag, _)| tag != tags::SIDE) {
            bad.push(tag, value);
        }
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
            (er.exec_type, er.ord_status, er.side),
            (fix::ExecType::Trade, fix::OrdStatus::PartiallyFilled, fix::Side::Sell)
        );
        assert_eq!((er.cl_ord_id.as_deref(), er.orig_cl_ord_id.as_deref()), (Some("C2"), Some("C1")));
        assert_eq!((er.last_qty, er.last_px), (Some(dec("40")), Some(dec("10.25"))));
        assert_eq!((er.leaves_qty, er.cum_qty, er.avg_px), (dec("60"), dec("40"), dec("10.25")));
        assert_eq!((er.symbol.as_deref(), er.account.as_deref()), (Some("AAPL"), Some("ACC")));
        assert_eq!((er.order_qty, er.price), (Some(dec("100")), Some(dec("10.5"))));
        assert_eq!(
            (er.ord_type, er.time_in_force),
            (Some(fix::OrdType::Limit), Some(fix::TimeInForce::GoodTillCancel))
        );
        assert_eq!((er.ord_rej_reason, er.text.as_deref(), er.transact_time), (None, Some("fill"), Some(time())));
    }

    #[test]
    fn a_report_without_a_fill_has_no_last_qty() {
        let er = client_report(ClientReport { exec_type: ExecType::New, last_qty: Decimal::ZERO, ..report() });
        assert_eq!((er.last_qty, er.last_px), (None, None));
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
            (er.exec_type, er.ord_rej_reason),
            (fix::ExecType::OrderStatus, Some(fix::OrdRejReason::UnknownOrder))
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
}
