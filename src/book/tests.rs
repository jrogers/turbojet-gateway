use std::collections::HashSet;
use std::time::Duration;

use turbojet::SessionId;

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
        (report.status, report.reject, report.order_id.as_deref()),
        (Status::Rejected, Some(RejectReason::NoRoute), None)
    );
    assert_eq!(report.exec_type, ExecType::Rejected);
    assert!(book.order(OrderId(1)).is_none());
}

#[test]
fn open_orders_are_counted_per_venue_until_done() {
    let mut book = book();
    new_order(&mut book, "C1");
    new_order(&mut book, "C2");
    assert_eq!(book.open_orders_by_venue(), BTreeMap::from([("NASDAQ".into(), 2)]));
    let failed = Input::SendFailed { venue: "NASDAQ".into(), downstream: "1".into(), text: "down".into() };
    run(&mut book, failed);
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Rejected);
    assert_eq!(book.open_orders_by_venue(), BTreeMap::from([("NASDAQ".into(), 1)]));
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
    assert_eq!((report.order_id.as_deref(), report.status, report.reject), (Some("1"), Status::PendingNew, None));
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
    let out = run(&mut book, Input::NewOrder { client: "FUNDB".into(), request: request("C1"), redelivered: false });
    assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
}

/// FUNDA's session with SubID `sub_id`.
fn desk(sub_id: &str) -> ClientId {
    ClientId::new(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA").with_target_sub_id(sub_id))
}

fn order_from(client: &ClientId, cl_ord_id: &str) -> Input {
    Input::NewOrder { client: client.clone(), request: request(cl_ord_id), redelivered: false }
}

#[test]
fn two_sessions_of_one_comp_id_have_their_own_cl_ord_ids_and_bounds() {
    let mut book = book();
    book.max_cl_ord_ids = 1;
    let (one, two) = (desk("DESK1"), desk("DESK2"));
    assert!(matches!(run(&mut book, order_from(&one, "C1")).as_slice(), [Output::NewOrder { .. }]));
    assert!(matches!(run(&mut book, order_from(&two, "C1")).as_slice(), [Output::NewOrder { .. }]));
    let out = run(&mut book, order_from(&one, "C2"));
    let [Output::Report { client, report }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((client, report.reject), (&one, Some(RejectReason::TooManyOrders)));
}

#[test]
fn reports_go_to_the_session_that_sent_the_order() {
    let mut book = book();
    let (one, two) = (desk("DESK1"), desk("DESK2"));
    run(&mut book, order_from(&one, "C1"));
    run(&mut book, order_from(&two, "C1"));
    for (downstream, sender) in [("1", &one), ("2", &two)] {
        let out = run(&mut book, venue_report(downstream, Status::New, 0, 0));
        let [Output::Report { client, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(client, sender);
    }
}

#[test]
fn a_route_rule_for_a_comp_id_matches_each_of_its_sessions() {
    let rule = |client: Option<&str>, venue: &str| Rule {
        client: client.map(Into::into),
        ex_destination: None,
        symbol: None,
        target: Target::Venue(venue.into()),
    };
    let mut book = Book::new(Router::new(vec![rule(Some("FUNDA"), "NASDAQ"), rule(None, "OTHER")]));
    for client in [desk("DESK1"), desk("DESK2"), "FUNDA".into()] {
        let out = run(&mut book, order_from(&client, "C1"));
        assert!(matches!(out.as_slice(), [Output::NewOrder { venue, .. }] if venue == "NASDAQ"), "{client}: {out:?}");
    }
    let out = run(&mut book, order_from(&"FUNDB".into(), "C1"));
    assert!(matches!(out.as_slice(), [Output::NewOrder { venue, .. }] if venue == "OTHER"), "{out:?}");
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
            status: Some(status),
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
    assert_eq!((client, report.cl_ord_id.as_str()), (&ClientId::from("FUNDA"), "C1"));
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
        redelivered: false,
    }
}

fn cancel_reject(out: &[Output]) -> (Option<&str>, CancelKind, Status, CancelRejectReason) {
    let [Output::CancelReject { order_id, response_to, status, reason, .. }] = out else { panic!("{out:?}") };
    (order_id.as_deref(), *response_to, *status, *reason)
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::Filled, CancelRejectReason::TooLate));
}

#[test]
fn a_cancel_reusing_a_cl_ord_id_is_refused() {
    let mut book = book();
    new_order(&mut book, "C1");
    let out = cancel(&mut book, "C1", "C1");
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::PendingNew, CancelRejectReason::Duplicate));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
}

#[test]
fn a_venue_cancel_reject_without_a_status_takes_the_order_out_of_pending() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report_as("1-1", ExecType::PendingCancel, Status::PendingCancel, 0, 0));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingCancel);
    let mut input = venue_cancel_reject("NASDAQ", "1-1", Status::New);
    let Input::VenueCancelReject { status, .. } = &mut input else { unreachable!() };
    *status = None;
    let out = run(&mut book, input);
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
}

fn venue_cancel_reject(venue: &str, downstream: &str, status: Status) -> Input {
    Input::VenueCancelReject {
        venue: venue.into(),
        downstream: downstream.into(),
        status: Some(status),
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
        redelivered: false,
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

/// FIX 4.2 counts a report as a fill only by its ExecType, so any report with a fill is Trade.
#[test]
fn a_status_report_with_a_fill_reaches_the_client_as_a_trade() {
    let mut book = book();
    new_order(&mut book, "C1");
    let out = run(&mut book, venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 40, 40));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.last_qty, report.cum_qty), (ExecType::Trade, 40.into(), 40.into()));
}

#[test]
fn a_replace_ack_with_a_fill_reaches_the_client_as_a_trade() {
    let mut book = book();
    new_order(&mut book, "C1");
    replace(&mut book, "C2", "C1", 80, 11);
    let out = run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::PartiallyFilled, 30, 30));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.last_qty, report.leaves_qty), (ExecType::Trade, 30.into(), 50.into()));
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((&order.request, order.pending.is_none()), (&replaced("C2", 80, 11), true));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::New, CancelRejectReason::Other));
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
        (Some("1"), CancelKind::Replace, Status::PendingNew, CancelRejectReason::AlreadyPending)
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
    assert_eq!(cancel_reject(&out), (None, CancelKind::Replace, Status::Rejected, CancelRejectReason::UnknownOrder));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::New, CancelRejectReason::Other));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::New, CancelRejectReason::Other));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
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
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::PendingNew, CancelRejectReason::Other));
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
    assert!(run(&mut book, with_venue_order_id(venue_report("1", Status::PartiallyFilled, 40, 40), "V-4")).is_empty());
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
    // A late report from before the replace is dropped, so doesn't roll it back.
    assert!(run(&mut book, with_venue_order_id(venue_report("1", Status::New, 0, 0), "V-1")).is_empty());
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
    let (Input::Cancel { side, symbol, .. } | Input::Replace { side, symbol, .. }) = &mut input else { unreachable!() };
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
                (text.as_str(), *reason, order_id.as_deref()),
                ("side or symbol does not match", CancelRejectReason::Other, Some("1"))
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
fn a_market_replace_without_a_price_goes_out_without_one() {
    let mut book = book();
    new_order(&mut book, "C1");
    let out = run(&mut book, to_market_ioc(replace_input("C2", "C1", 80, 0)));
    let [Output::Replace { request, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((request.ord_type, request.price), (OrdType::Market, None));
}

#[test]
fn a_market_order_replaced_to_limit_reaches_the_venue_with_its_price() {
    let mut book = book();
    let market = OrderRequest { ord_type: OrdType::Market, price: None, ..request("C1") };
    run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: market.clone(), redelivered: false });
    let mut input = replace_input("C2", "C1", 80, 11);
    let Input::Replace { ord_type, .. } = &mut input else { unreachable!() };
    *ord_type = Some(OrdType::Limit);
    let out = run(&mut book, input);
    let [Output::Replace { request, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(request, &replaced("C1", 80, 11));
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert_eq!(book.order(OrderId(1)).unwrap().request, replaced("C2", 80, 11));
}

#[test]
fn a_replace_keeping_time_in_force_leaves_gtc() {
    let mut book = book();
    let gtc = OrderRequest { time_in_force: Some(TimeInForce::Gtc), ..request("C1") };
    run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: gtc, redelivered: false });
    let out = replace(&mut book, "C2", "C1", 80, 11);
    let [Output::Replace { request, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(request.time_in_force, Some(TimeInForce::Gtc));
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert_eq!(book.order(OrderId(1)).unwrap().request.time_in_force, Some(TimeInForce::Gtc));
}

/// Which downstream ClOrdID a report in `stale_rules` comes under.
#[derive(Debug, Clone, Copy)]
enum Under {
    /// The order's current one, "1".
    Current,
    /// The pending replace's, "1-2".
    Pending,
    /// An earlier request's, "1-1".
    Other,
}

/// Whether `stale` drops a report without a fill, with `report` status, under `under`, whose
/// request the venue has `answered`, for an order with `status` and, if `pending`, a replace
/// pending under "1-2".
fn dropped(status: Status, report: Status, under: Under, answered: bool, pending: bool) -> bool {
    let order = Order {
        id: OrderId(1),
        name: "1".into(),
        client: "FUNDA".into(),
        venue: "NASDAQ".into(),
        request: request("C1"),
        downstream: "1".into(),
        amendments: 2,
        status,
        cum_qty: Decimal::ZERO,
        avg_px: Decimal::ZERO,
        pending: pending.then(|| Pending {
            downstream: "1-2".into(),
            cl_ord_id: "C3".into(),
            qty: Decimal::from(80),
            price: Some(Decimal::from(11)),
            ord_type: OrdType::Limit,
            time_in_force: None,
        }),
        exec_seq: 0,
        venue_order_id: None,
        cancel: None,
        gateway_cancel: None,
    };
    let downstream = match under {
        Under::Current => "1",
        Under::Pending => "1-2",
        Under::Other => "1-1",
    };
    let Input::VenueReport { report, .. } = venue_report(downstream, report, 0, 0) else { unreachable!() };
    stale(&order, answered, &report)
}

#[test]
fn stale_rules() {
    use Status::{Canceled, Filled, New, PartiallyFilled, PendingCancel, PendingNew, PendingReplace};
    use Under::{Current, Other, Pending};
    let table = [
        // Done is final.
        (Filled, New, Current, false, true),
        // PendingNew only while the order is.
        (PendingNew, PendingNew, Current, false, false),
        (New, PendingNew, Current, false, true),
        (PartiallyFilled, PendingNew, Current, false, true),
        (PendingNew, New, Current, false, false),
        // A pending state may follow New or PartiallyFilled.
        (New, PendingCancel, Other, false, false),
        (PartiallyFilled, PendingCancel, Other, false, false),
        (PartiallyFilled, PendingReplace, Pending, false, false),
        // New while a request is pending is a resend, unless it accepts the pending replace.
        (PendingCancel, New, Current, false, true),
        (PendingReplace, New, Current, false, true),
        (PendingCancel, New, Other, false, true),
        (PendingReplace, New, Pending, false, false),
        (PendingCancel, New, Pending, false, false),
        (PendingCancel, Canceled, Other, false, false),
        // So is PartiallyFilled without a fill: the order's state from before the request.
        (PendingCancel, PartiallyFilled, Current, false, true),
        (PendingReplace, PartiallyFilled, Current, true, true),
        (PendingReplace, PartiallyFilled, Pending, false, false),
        // A settled request's reports, except on the order's current downstream.
        (New, PendingReplace, Other, true, true),
        (New, PendingCancel, Other, true, true),
        (New, New, Other, true, true),
        // Done can't be a rollback: the venue ended the order, whatever it came under.
        (New, Canceled, Other, true, false),
        (New, Canceled, Current, true, false),
        (New, New, Current, true, false),
        // On the current downstream, the accepted replace's own pending state is a resend.
        (New, PendingReplace, Current, true, true),
        // Unsolicited.
        (New, Canceled, Current, false, false),
        (PartiallyFilled, Canceled, Current, false, false),
    ];
    for (status, report, under, answered, expected) in table {
        assert_eq!(
            dropped(status, report, under, answered, true),
            expected,
            "{status:?} {report:?} {under:?} {answered}"
        );
    }
    // Nothing pending: a cancel is, or nothing is.
    let table = [
        (PendingCancel, New, Current, false, true),
        (PendingCancel, PartiallyFilled, Current, false, true),
        (PendingCancel, PendingCancel, Other, false, false),
        (PendingCancel, Canceled, Other, false, false),
        (New, Canceled, Current, false, false),
        (New, PendingReplace, Current, true, true),
    ];
    for (status, report, under, answered, expected) in table {
        assert_eq!(
            dropped(status, report, under, answered, false),
            expected,
            "{status:?} {report:?} {under:?} {answered}, nothing pending"
        );
    }
}

#[test]
fn a_resent_replace_acceptance_while_another_replace_is_pending_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::PartiallyFilled, 0, 40));
    replace(&mut book, "C3", "C2", 70, 11);
    run(&mut book, venue_report("1-2", Status::PendingReplace, 0, 40));
    let resent = venue_report_as("1-1", ExecType::Replaced, Status::PartiallyFilled, 0, 40);
    assert!(run(&mut book, resent).is_empty());
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((order.status, order.request.qty), (Status::PendingReplace, 80.into()));
    assert!(order.pending.is_some());
}

#[test]
fn an_unsolicited_cancel_under_a_rejected_cancels_downstream_is_applied() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
    let out = run(&mut book, venue_report("1-1", Status::Canceled, 0, 0));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(report.status, Status::Canceled);
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Canceled);
}

#[test]
fn a_late_new_under_a_replaced_downstream_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert!(run(&mut book, venue_report("1", Status::New, 0, 0)).is_empty());
    // Fills under it still count.
    assert_eq!(run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40)).len(), 1);
}

#[test]
fn a_late_pending_replace_after_acceptance_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report("1-1", Status::PendingReplace, 0, 0));
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert!(run(&mut book, venue_report("1-1", Status::PendingReplace, 0, 0)).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
}

#[test]
fn a_pending_cancel_after_a_partial_fill_is_applied() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    cancel(&mut book, "C2", "C1");
    let out = run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 40));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.status, report.cl_ord_id.as_str()), (Status::PendingCancel, "C2"));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingCancel);
}

#[test]
fn an_unsolicited_cancel_on_the_current_downstream_is_applied() {
    let mut book = book();
    new_order(&mut book, "C1");
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let out = run(&mut book, venue_report("1-1", Status::Canceled, 0, 0));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.status, report.cl_ord_id.as_str(), &report.orig_cl_ord_id), (Status::Canceled, "C2", &None));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Canceled);
}

#[test]
fn a_late_new_while_a_cancel_is_pending_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0));
    assert!(run(&mut book, venue_report("1", Status::New, 0, 0)).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingCancel);
}

#[test]
fn a_late_pending_cancel_after_the_cancel_is_rejected_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0));
    run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
    assert!(run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0)).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
}

#[test]
fn a_late_pending_new_after_the_ack_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    assert!(run(&mut book, venue_report("1", Status::PendingNew, 0, 0)).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
}

/// `input`, a cancel or replace, flagged as possibly redelivered.
fn redelivered(mut input: Input) -> Input {
    let (Input::Cancel { redelivered, .. } | Input::Replace { redelivered, .. }) = &mut input else { unreachable!() };
    *redelivered = true;
    input
}

#[test]
fn a_redelivered_cancel_gets_the_orders_state_not_a_second_cancel() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0));
    let out = run(&mut book, redelivered(cancel_input("C2", "C1")));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(
        (report.exec_type, report.status, report.order_id.as_deref()),
        (ExecType::Status, Status::PendingCancel, Some("1"))
    );
    // It answers the cancel, under the cancel's own ClOrdIDs.
    assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
    assert_eq!(book.order(OrderId(1)).unwrap().amendments, 1);
}

#[test]
fn a_redelivered_replace_gets_the_orders_state_not_a_second_replace() {
    let mut book = book();
    new_order(&mut book, "C1");
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let out = run(&mut book, redelivered(replace_input("C2", "C1", 80, 11)));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.status, report.request.qty), (ExecType::Status, Status::New, 80.into()));
    assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
    assert_eq!(book.order(OrderId(1)).unwrap().amendments, 1);
}

#[test]
fn a_reused_cl_ord_id_on_a_cancel_or_replace_is_a_duplicate_unless_redelivered_for_the_same_order() {
    for input in [cancel_input("C2", "C1"), replace_input("C2", "C1", 80, 11)] {
        let mut book = book();
        new_order(&mut book, "C1");
        new_order(&mut book, "C2");
        // Not redelivered: a duplicate even for the order it names.
        cancel(&mut book, "C3", "C1");
        let mut again = input.clone();
        let (Input::Cancel { cl_ord_id, .. } | Input::Replace { cl_ord_id, .. }) = &mut again else { unreachable!() };
        *cl_ord_id = "C3".into();
        assert_eq!(cancel_reject(&run(&mut book, again.clone())).3, CancelRejectReason::Duplicate);
        // Redelivered, but C2 is another order's ClOrdID.
        assert_eq!(cancel_reject(&run(&mut book, redelivered(input))).3, CancelRejectReason::Duplicate);
        // Redelivered for the same order.
        assert!(matches!(run(&mut book, redelivered(again)).as_slice(), [Output::Report { .. }]));
    }
}

fn status_request(cl_ord_id: &str) -> Input {
    Input::StatusRequest { client: "FUNDA".into(), cl_ord_id: cl_ord_id.into(), side: Side::Buy, symbol: "AAPL".into() }
}

#[test]
fn a_status_request_gets_the_orders_state() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    let out = run(&mut book, status_request("C1"));
    let [Output::Report { client, report }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(*client, ClientId::from("FUNDA"));
    assert_eq!((report.exec_type, report.status, report.reject), (ExecType::Status, Status::PartiallyFilled, None));
    assert_eq!((report.order_id.as_deref(), report.cum_qty, report.leaves_qty), (Some("1"), 40.into(), 60.into()));
    assert_eq!((report.cl_ord_id.as_str(), &report.orig_cl_ord_id, report.exec_id.as_str()), ("C1", &None, "1.2"));
}

#[test]
fn a_status_request_after_a_replace_names_the_current_cl_ord_id() {
    let mut book = book();
    new_order(&mut book, "C1");
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let out = run(&mut book, status_request("C1"));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", Some("C1")));
    assert_eq!((report.exec_type, report.request.qty), (ExecType::Status, 80.into()));
}

#[test]
fn a_status_request_for_an_unknown_order_is_answered_rejected() {
    let mut book = book();
    new_order(&mut book, "C1");
    new_order(&mut book, "C1");
    let out = run(&mut book, status_request("C9"));
    let [Output::Report { client, report }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(*client, ClientId::from("FUNDA"));
    assert_eq!((report.exec_type, report.status, report.reject), (ExecType::Status, Status::Rejected, None));
    assert_eq!((report.order_id.as_deref(), report.cl_ord_id.as_str()), (None, "C9"));
    assert_eq!((report.text.as_deref(), report.exec_id.as_str()), (Some("unknown order"), "R2"));
    assert_eq!((report.request.side, report.request.symbol.as_str()), (Side::Buy, "AAPL"));
    assert_eq!((report.cum_qty, report.leaves_qty), (Decimal::ZERO, Decimal::ZERO));
}

#[test]
fn a_rejected_replace_leaves_ord_type_and_time_in_force() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, to_market_ioc(replace_input("C2", "C1", 80, 0)));
    run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
    assert_eq!(book.order(OrderId(1)).unwrap().request, request("C1"));
}

fn send_failed(downstream: &str) -> Input {
    Input::SendFailed { venue: "NASDAQ".into(), downstream: downstream.into(), text: "session not connected".into() }
}

#[test]
fn a_new_order_the_venue_session_could_not_take_is_rejected() {
    let mut book = book();
    new_order(&mut book, "C1");
    let out = run(&mut book, send_failed("1"));
    let [Output::Report { client, report }] = out.as_slice() else { panic!("{out:?}") };
    let funda = ClientId::from("FUNDA");
    assert_eq!((client, report.cl_ord_id.as_str(), report.order_id.as_deref()), (&funda, "C1", Some("1")));
    assert_eq!((report.exec_type, report.status), (ExecType::Rejected, Status::Rejected));
    assert_eq!(
        (report.reject, report.text.as_deref()),
        (Some(RejectReason::VenueUnavailable), Some("session not connected"))
    );
    assert_eq!((report.exec_id.as_str(), report.leaves_qty), ("1.1", Decimal::ZERO));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Rejected);
    let out = cancel(&mut book, "C2", "C1");
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::Rejected, CancelRejectReason::TooLate));
}

#[test]
fn a_cancel_the_venue_session_could_not_take_is_refused() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel(&mut book, "C2", "C1");
    let out = run(&mut book, send_failed("1-1"));
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::New, CancelRejectReason::Other));
    let [Output::CancelReject { cl_ord_id, orig_cl_ord_id, text, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((cl_ord_id.as_str(), orig_cl_ord_id.as_str(), text.as_str()), ("C2", "C1", "session not connected"));
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((order.status, &order.request, order.downstream.as_str()), (Status::New, &request("C1"), "1"));
}

#[test]
fn a_replace_the_venue_session_could_not_take_is_refused_and_another_goes_out() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 80, 11);
    let out = run(&mut book, send_failed("1-1"));
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::New, CancelRejectReason::Other));
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((&order.request, order.pending.is_none()), (&request("C1"), true));
    assert!(matches!(replace(&mut book, "C3", "C1", 70, 11).as_slice(), [Output::Replace { .. }]));
}

/// The client's cancel gets an answer even when the order was rejected first: the venue
/// never saw the cancel, so it won't answer it either.
#[test]
fn a_cancel_the_venue_session_could_not_take_is_refused_after_its_order_is_rejected() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel(&mut book, "C2", "C1");
    run(&mut book, send_failed("1"));
    let out = run(&mut book, send_failed("1-1"));
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::Rejected, CancelRejectReason::Other));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::Rejected);
}

#[test]
fn a_send_failure_for_an_unknown_request_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    assert!(run(&mut book, send_failed("9")).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingNew);
}

#[test]
fn a_second_send_failure_for_the_same_request_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, send_failed("1"));
    assert!(run(&mut book, send_failed("1")).is_empty());
    new_order(&mut book, "C2");
    cancel(&mut book, "C3", "C2");
    run(&mut book, send_failed("2-1"));
    assert!(run(&mut book, send_failed("2-1")).is_empty());
}

#[test]
fn a_failed_send_for_an_acked_order_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    assert!(run(&mut book, send_failed("1")).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
}

#[test]
fn a_failed_cancel_leaves_a_pending_replace_pending() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report("1-1", Status::PendingReplace, 0, 0));
    cancel(&mut book, "C3", "C1");
    let out = run(&mut book, send_failed("1-2"));
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Cancel, Status::PendingReplace, CancelRejectReason::Other));
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((order.status, order.pending.is_some()), (Status::PendingReplace, true));
}

#[test]
fn a_failed_replace_leaves_a_pending_cancel_pending() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0));
    replace(&mut book, "C3", "C1", 80, 11);
    let out = run(&mut book, send_failed("1-2"));
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::PendingCancel, CancelRejectReason::Other));
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((order.status, order.pending.is_none()), (Status::PendingCancel, true));
}

#[test]
fn replaced_routes_apply_to_new_orders_only() {
    let mut book = book();
    new_order(&mut book, "C1");
    book.set_router(Router::new(vec![Rule {
        client: None,
        ex_destination: None,
        symbol: None,
        target: Target::Venue("NYSE".into()),
    }]));
    let out = new_order(&mut book, "C2");
    assert!(matches!(out.as_slice(), [Output::NewOrder { venue, .. }] if venue == "NYSE"), "{out:?}");
    let out = cancel(&mut book, "C3", "C1");
    assert!(matches!(out.as_slice(), [Output::Cancel { venue, .. }] if venue == "NASDAQ"), "{out:?}");
}

/// `input`, a venue report, as ExecType `exec_type` keeping the order's status.
fn keeping_status(mut input: Input, exec_type: ExecType) -> Input {
    let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
    (report.status, report.exec_type) = (None, exec_type);
    input
}

#[test]
fn a_done_for_day_keeping_the_status_is_forwarded_with_the_orders() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    let out = run(&mut book, keeping_status(venue_report("1", Status::New, 0, 40), ExecType::DoneForDay));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.status, report.reject), (ExecType::DoneForDay, Status::PartiallyFilled, None));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PartiallyFilled);
}

#[test]
fn a_report_keeping_the_status_does_not_accept_a_pending_replace() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 80, 11);
    let out = run(&mut book, keeping_status(venue_report("1-1", Status::New, 0, 0), ExecType::DoneForDay));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.status, report.request.qty), (Status::New, 100.into()));
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((order.status, &order.request, order.pending.is_some()), (Status::New, &request("C1"), true));
}

#[test]
fn exec_type_rejected_keeping_the_status_rejects_nothing() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    let out = run(&mut book, keeping_status(venue_report("1", Status::New, 0, 0), ExecType::Rejected));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.status, report.reject), (Status::New, None));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::New);
}

const PREFIX: &str = "0K3J9X";

fn prefixed() -> Book {
    let mut book = book();
    book.set_prefix(PREFIX.into());
    book
}

#[test]
fn a_prefixed_book_puts_its_prefix_on_every_id() {
    let mut book = prefixed();
    let [Output::NewOrder { downstream, .. }] = &new_order(&mut book, "C1")[..] else { panic!() };
    assert_eq!(downstream, "0K3J9X1");
    let out = run(&mut book, venue_report("0K3J9X1", Status::New, 0, 0));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.order_id.as_deref(), report.exec_id.as_str()), (Some("0K3J9X1"), "0K3J9X1.1"));
    let out = cancel(&mut book, "C2", "C1");
    let [Output::Cancel { downstream, orig_downstream, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((downstream.as_str(), orig_downstream.as_str()), ("0K3J9X1-1", "0K3J9X1"));
    assert_eq!(exec_ids(&new_order(&mut book, "C1")), ["0K3J9XR1"]);
}

/// A late report or a resend from before a restart names an order of the last start's.
#[test]
fn a_report_for_an_order_of_an_earlier_start_is_unknown() {
    let mut book = prefixed();
    new_order(&mut book, "C1");
    assert!(run(&mut book, venue_report("1", Status::Filled, 100, 100)).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingNew);
}

#[test]
fn books_started_a_second_apart_share_no_id() {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(ID_EPOCH + 1_000_000);
    let ids = |start| {
        let mut book = book();
        book.set_prefix(start_prefix(start));
        let mut ids = HashSet::new();
        for n in 0..20 {
            let [Output::NewOrder { downstream, .. }] = &new_order(&mut book, &format!("C{n}"))[..] else { panic!() };
            ids.insert(downstream.clone());
        }
        ids
    };
    assert!(ids(start).is_disjoint(&ids(start + Duration::from_secs(1))));
}

#[test]
fn the_start_prefix_is_six_base36_digits_of_seconds_since_2026() {
    let at = |secs| start_prefix(SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
    assert_eq!(at(ID_EPOCH), "000000");
    assert_eq!(at(ID_EPOCH + 35), "00000Z");
    assert_eq!(at(ID_EPOCH + 36), "000010");
    // 2026-10-10T00:00:00Z.
    assert_eq!(at(1_791_590_400), "0EI800");
    assert_eq!(at(0), "000000", "a clock before 2026");
}

fn cancel_all(book: &mut Book, scope: Scope) -> Vec<Output> {
    run(book, Input::CancelAll { scope })
}

/// The downstream ClOrdIDs of the cancels in `out`, which must hold only cancels.
fn cancels(out: &[Output]) -> Vec<&str> {
    out.iter()
        .map(|output| match output {
            Output::Cancel { downstream, .. } => downstream.as_str(),
            _ => panic!("{out:?}"),
        })
        .collect()
}

/// What `cancelling` says, as (order, tries, unsent, stuck).
fn cancelling(book: &Book) -> Vec<(u64, u32, bool, bool)> {
    book.cancelling().into_iter().map(|(id, gateway)| (id.0, gateway.tries, gateway.unsent, gateway.stuck)).collect()
}

fn cancel_reject_because(downstream: &str, status: Status, reason: u32) -> Input {
    let mut input = venue_cancel_reject("NASDAQ", downstream, status);
    let Input::VenueCancelReject { reason: why, .. } = &mut input else { unreachable!() };
    *why = Some(reason);
    input
}

fn venue_up(venue: &str) -> Input {
    Input::VenueUp { venue: venue.into() }
}

#[test]
fn cancel_all_covers_its_scope_only() {
    let mut book = book();
    let (one, two) = (desk("DESK1"), desk("DESK2"));
    run(&mut book, order_from(&one, "C1"));
    run(&mut book, order_from(&two, "C1"));
    run(&mut book, order_from(&"FUNDB".into(), "C1"));
    assert_eq!(cancels(&cancel_all(&mut book, Scope::Session(one))), ["1-1"]);
    // The first is already being cancelled.
    assert_eq!(cancels(&cancel_all(&mut book, Scope::CompId("FUNDA".into()))), ["2-1"]);
    assert_eq!(cancels(&cancel_all(&mut book, Scope::All)), ["3-1"]);
    assert_eq!(cancelling(&book), [(1, 1, false, false), (2, 1, false, false), (3, 1, false, false)]);
}

#[test]
fn cancel_all_again_leaves_a_cancel_in_flight_alone() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel_all(&mut book, Scope::All);
    assert!(cancel_all(&mut book, Scope::All).is_empty());
    assert_eq!(cancelling(&book), [(1, 1, false, false)]);
}

#[test]
fn cancel_all_skips_done_orders_and_waits_out_client_cancels_but_not_replaces() {
    let mut book = book();
    for cl_ord_id in ["C1", "C2", "C3", "C4"] {
        new_order(&mut book, cl_ord_id);
    }
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    cancel(&mut book, "X2", "C2");
    replace(&mut book, "X4", "C4", 50, 10);
    let out = cancel_all(&mut book, Scope::All);
    assert_eq!(cancels(&out), ["3-1", "4-2"]);
    // The replace isn't accepted: the venue knows the order by its original.
    let Output::Cancel { orig_downstream, request: sent, .. } = &out[1] else { unreachable!() };
    assert_eq!((orig_downstream.as_str(), sent), ("4", &request("C4")));
    // The client's cancel may do it; the gateway's waits.
    assert_eq!(cancelling(&book), [(2, 0, true, false), (3, 1, false, false), (4, 1, false, false)]);
}

#[test]
fn a_gateway_cancel_waiting_on_a_client_cancel_goes_when_the_venue_refuses_that() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel(&mut book, "C2", "C1");
    assert!(cancel_all(&mut book, Scope::All).is_empty());
    let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
    let [Output::CancelReject { .. }, Output::Cancel { downstream, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(downstream, "1-2");
    assert_eq!(cancelling(&book), [(1, 1, false, false)]);
}

#[test]
fn a_gateway_cancel_waiting_on_a_client_cancel_that_failed_to_send_goes_when_the_venue_logs_on() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel(&mut book, "C2", "C1");
    cancel_all(&mut book, Scope::All);
    let out = run(&mut book, send_failed("1-1"));
    assert!(matches!(out.as_slice(), [Output::CancelReject { .. }]), "{out:?}");
    assert_eq!(cancels(&run(&mut book, venue_up("NASDAQ"))), ["1-2"]);
}

#[test]
fn the_venues_cancel_reaches_the_client_unsolicited_under_its_current_cl_ord_id() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 50, 10);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert_eq!(cancels(&run(&mut book, Input::CancelOrder { order: OrderId(1) })), ["1-2"]);
    let out = run(&mut book, venue_report("1-2", Status::Canceled, 0, 0));
    let [Output::Report { client, report }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(client, &ClientId::from("FUNDA"));
    assert_eq!((report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()), ("C2", None));
    assert_eq!((report.exec_type, report.status), (ExecType::Canceled, Status::Canceled));
    assert!(book.cancelling().is_empty());
}

#[test]
fn a_canceled_under_the_orders_own_downstream_ends_a_gateway_cancel() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel_all(&mut book, Scope::All);
    let out = run(&mut book, venue_report("1", Status::Canceled, 0, 0));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.cl_ord_id.as_str(), report.exec_type), ("C1", ExecType::Canceled));
    assert!(book.cancelling().is_empty());
    assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::Canceled)).is_empty());
}

#[test]
fn only_fills_and_the_end_under_a_gateway_cancel_reach_the_client() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel_all(&mut book, Scope::All);
    // The client never asked; after a refusal it would be left showing PendingCancel.
    assert!(run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 0)).is_empty());
    let out = run(&mut book, venue_report("1-1", Status::PendingCancel, 40, 40));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.cl_ord_id.as_str(), report.exec_type, report.last_qty), ("C1", ExecType::Trade, 40.into()));
}

#[test]
fn a_refused_gateway_cancel_is_retried_until_stuck_and_never_reaches_the_client() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    cancel_all(&mut book, Scope::All);
    assert_eq!(cancels(&run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New))), ["1-2"]);
    // A Rejected report under the cancel is a refusal too.
    let rejected = venue_report_as("1-2", ExecType::Rejected, Status::New, 0, 0);
    assert_eq!(cancels(&run(&mut book, rejected)), ["1-3"]);
    assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-3", Status::New)).is_empty());
    assert_eq!(cancelling(&book), [(1, MAX_GATEWAY_CANCEL_TRIES, false, true)]);
    // Stuck, the gateway stands aside for the client's own cancel.
    assert_eq!(cancels(&cancel(&mut book, "C2", "C1")), ["1-4"]);
    // An operator asking again gets another round.
    run(&mut book, venue_cancel_reject("NASDAQ", "1-4", Status::New));
    assert_eq!(cancels(&cancel_all(&mut book, Scope::All)), ["1-5"]);
    assert_eq!(cancelling(&book), [(1, 1, false, false)]);
}

#[test]
fn too_late_or_unknown_ends_the_gateway_cancel_without_a_retry() {
    let mut book = book();
    new_order(&mut book, "C1");
    new_order(&mut book, "C2");
    cancel_all(&mut book, Scope::All);
    // The venue says it's too late and the order filled: it's done, nothing to cancel.
    assert!(run(&mut book, cancel_reject_because("1-1", Status::Filled, 0)).is_empty());
    // Unknown to the venue but live here: stuck, for someone to look at.
    assert!(run(&mut book, cancel_reject_because("2-1", Status::New, 1)).is_empty());
    assert_eq!(cancelling(&book), [(2, 1, false, true)]);
}

#[test]
fn unknown_after_a_replace_was_accepted_meanwhile_is_retried_against_it() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 50, 10);
    cancel_all(&mut book, Scope::All);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    // The cancel named "1", which the replace superseded.
    let out = run(&mut book, cancel_reject_because("1-2", Status::New, 1));
    let [Output::Cancel { downstream, orig_downstream, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((downstream.as_str(), orig_downstream.as_str()), ("1-3", "1-1"));
    assert_eq!(cancelling(&book), [(1, 2, false, false)]);
}

#[test]
fn a_flapping_venue_never_uses_up_the_gateways_tries() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel_all(&mut book, Scope::All);
    assert!(run(&mut book, send_failed("1-1")).is_empty());
    assert_eq!(cancelling(&book), [(1, 0, true, false)]);
    assert!(run(&mut book, venue_up("LSE")).is_empty());
    for n in 2..=MAX_GATEWAY_CANCEL_TRIES + 2 {
        let downstream = format!("1-{n}");
        assert_eq!(cancels(&run(&mut book, venue_up("NASDAQ"))), [downstream.as_str()]);
        assert!(run(&mut book, venue_up("NASDAQ")).is_empty(), "one in flight");
        assert_eq!(cancelling(&book), [(1, 1, false, false)]);
        run(&mut book, send_failed(&downstream));
    }
    assert_eq!(cancelling(&book), [(1, 0, true, false)]);
}

#[test]
fn client_requests_during_a_gateway_cancel_are_refused_as_pending() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel_all(&mut book, Scope::All);
    let pending = (Some("1"), CancelKind::Cancel, Status::PendingNew, CancelRejectReason::AlreadyPending);
    assert_eq!(cancel_reject(&cancel(&mut book, "C2", "C1")), pending);
    let pending = (Some("1"), CancelKind::Replace, Status::PendingNew, CancelRejectReason::AlreadyPending);
    assert_eq!(cancel_reject(&replace(&mut book, "C3", "C1", 50, 10)), pending);
}

#[test]
fn gateway_cancels_spend_none_of_the_clients_cl_ord_ids() {
    let mut book = book();
    new_order(&mut book, "C1");
    let client = ClientId::from("FUNDA");
    let before = (book.used[&client], book.index.len());
    cancel_all(&mut book, Scope::All);
    run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
    assert_eq!((book.used[&client], book.index.len()), before);
}

#[test]
fn cancelling_an_unknown_or_done_order_does_nothing() {
    let mut book = book();
    assert!(run(&mut book, Input::CancelOrder { order: OrderId(1) }).is_empty());
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    assert!(run(&mut book, Input::CancelOrder { order: OrderId(1) }).is_empty());
    assert!(book.cancelling().is_empty());
}
