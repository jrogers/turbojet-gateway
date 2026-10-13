use std::collections::HashSet;
use std::time::Duration;

use turbojet::SessionId;

use super::report::stale;
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

/// Every test's inputs go through here, so each checks the book's persistence too: a book
/// restored from the changes taken before the input answers it as the live one does, and one
/// restored from the changes taken after it holds what the live one holds.
fn run(book: &mut Book, input: Input) -> Vec<Output> {
    let mut before = restored(book);
    let mut out = Vec::new();
    book.handle(input.clone(), &mut out);
    let mut again = Vec::new();
    before.handle(input, &mut again);
    assert_eq!(again, out, "the restored book's answer");
    assert_eq!(before.snapshot(), book.snapshot(), "what the restored book holds after the input");
    let changes = book.take_changes();
    book.mirror.apply(changes);
    let after = restored(book);
    assert_eq!(after.snapshot(), book.snapshot(), "what the restored book holds");
    assert_eq!(after.used, book.used);
    out
}

/// A new book restored from what `book` has had taken, set up as `book` is.
fn restored(book: &Book) -> Book {
    let mut restored = Book::new(book.router.clone());
    restored.set_config(book.router.clone(), book.limits.clone());
    restored.max_cl_ord_ids = book.max_cl_ord_ids;
    restored.amend_reserve = book.amend_reserve;
    restored.unavailable = book.unavailable;
    restored.now = book.now;
    restored.set_prefix(book.prefix.clone());
    restored.restore(book.mirror.clone());
    // A restart numbers rejects afresh under a new prefix; this one keeps the live book's prefix
    // and count, so their answers compare. OrderIds carry on from the snapshot.
    assert_eq!(restored.next_id, book.next_id);
    restored.rejects = book.rejects;
    restored
}

fn new_order(book: &mut Book, cl_ord_id: &str) -> Vec<Output> {
    run(book, Input::NewOrder { client: "FUNDA".into(), request: request(cl_ord_id), redelivered: false, sent: None })
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
    let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: req, redelivered: false, sent: None });
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
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true, sent: None },
    );
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
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true, sent: None },
    );
    assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
}

#[test]
fn a_redelivery_for_a_different_order_is_still_a_duplicate() {
    let mut book = book();
    new_order(&mut book, "C1");
    let mut other = request("C1");
    other.side = Side::Sell;
    let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: other, redelivered: true, sent: None });
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(report.reject, Some(RejectReason::Duplicate));
}

#[test]
fn the_same_cl_ord_id_from_another_client_is_a_new_order() {
    let mut book = book();
    new_order(&mut book, "C1");
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDB".into(), request: request("C1"), redelivered: false, sent: None },
    );
    assert!(matches!(out.as_slice(), [Output::NewOrder { .. }]));
}

/// FUNDA's session with SubID `sub_id`.
fn desk(sub_id: &str) -> ClientId {
    ClientId::new(&SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDA").with_target_sub_id(sub_id))
}

fn order_from(client: &ClientId, cl_ord_id: &str) -> Input {
    Input::NewOrder { client: client.clone(), request: request(cl_ord_id), redelivered: false, sent: None }
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

/// A venue repeating a report that changes nothing (an ack again, at the order's status and
/// CumQty, on its current downstream) reaches the client no more, though its VenueOrderID is
/// taken if the book had none; a restatement at the same status still goes.
#[test]
fn a_report_changing_nothing_is_dropped() {
    let mut book = book();
    new_order(&mut book, "C1");
    assert_eq!(run(&mut book, venue_report("1", Status::New, 0, 0)).len(), 1);
    let mut again = venue_report("1", Status::New, 0, 0);
    if let Input::VenueReport { report, .. } = &mut again {
        report.venue_order_id = Some("V-1".into());
    }
    assert!(run(&mut book, again).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().venue_order_id.as_deref(), Some("V-1"));
    assert_eq!(run(&mut book, venue_report_as("1", ExecType::Restated, Status::New, 0, 0)).len(), 1);
}

/// DoneForDay with the order's status given explicitly, unchanged (150=3, 39=0), still reaches
/// the client: it's the end of the order's day.
#[test]
fn done_for_day_with_an_unchanged_status_is_forwarded() {
    let mut book = acked();
    let out = run(&mut book, venue_report_as("1", ExecType::DoneForDay, Status::New, 0, 0));
    assert_eq!(report_of(&out).exec_type, ExecType::DoneForDay);
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

/// `input`, a venue report, with LastPx `last_px` and AvgPx `avg_px`.
fn priced(mut input: Input, last_px: &str, avg_px: &str) -> Input {
    let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
    report.last_px = last_px.parse().unwrap();
    report.avg_px = avg_px.parse().unwrap();
    input
}

/// A Status report raising CumQty without a LastQty carries a fill the gateway missed: the client
/// gets it as a Trade, priced from the change in AvgPx (40 at 10, then 60 at 10.5: 20 at 11.5).
#[test]
fn a_fill_the_gateway_missed_reaches_the_client_as_a_trade_at_the_implied_price() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    let missed = venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 0, 60);
    let out = run(&mut book, priced(missed, "0", "10.5"));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(
        (report.exec_type, report.last_qty, report.last_px),
        (ExecType::Trade, 20.into(), "11.5".parse().unwrap())
    );
    assert_eq!((report.cum_qty, report.avg_px), (60.into(), "10.5".parse().unwrap()));
    // A resend of the missed fill, or of part of it, is behind the order's CumQty.
    assert!(run(&mut book, venue_report("1", Status::PartiallyFilled, 20, 60)).is_empty());
    assert!(run(&mut book, venue_report("1", Status::PartiallyFilled, 10, 50)).is_empty());
}

/// A fill report raising CumQty by more than its LastQty carries the gap too: the client gets the
/// whole rise, priced from AvgPx (40 at 10, then 70 at 11.2: 30 at 12.8).
#[test]
fn a_fill_after_a_gap_reaches_the_client_as_the_whole_rise() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    let out = run(&mut book, priced(venue_report("1", Status::PartiallyFilled, 10, 70), "12", "11.2"));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(
        (report.exec_type, report.last_qty, report.last_px),
        (ExecType::Trade, 30.into(), "12.8".parse().unwrap())
    );
}

/// An implied price that doesn't divide out is rounded: 40 at 10, then 70 at 11, is 30 at 12.33...
#[test]
fn an_implied_price_is_rounded_to_eight_places() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    let out = run(&mut book, priced(venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 0, 70), "0", "11"));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(report.last_px, "12.33333333".parse().unwrap());
}

/// The downstream ClOrdID the gateway's status request for order 1 names; `None` if it sends none.
fn venue_status_request(book: &mut Book) -> Option<CompactString> {
    let before = book.snapshot();
    let out = run(book, Input::VenueStatusRequest { order: OrderId(1) });
    // It records nothing: no `Sent`, no new downstream number.
    assert_eq!(book.snapshot(), before);
    assert!(book.take_changes().is_empty());
    match out.as_slice() {
        [] => None,
        [Output::StatusRequest { venue, downstream, request, venue_order_id }] => {
            assert_eq!((venue.as_str(), request.side, request.symbol.as_str()), ("NASDAQ", Side::Buy, "AAPL"));
            assert_eq!(*venue_order_id, book.order(OrderId(1)).unwrap().venue_order_id);
            Some(downstream.clone())
        }
        _ => panic!("{out:?}"),
    }
}

/// The gateway asks under the ClOrdID the venue last acknowledged: the original while a replace is
/// pending, the replace's once it's accepted. Nothing for an unknown or done order.
#[test]
fn a_status_request_names_the_downstream_the_venue_last_acknowledged() {
    let mut book = book();
    assert_eq!(venue_status_request(&mut book), None);
    new_order(&mut book, "C1");
    assert_eq!(venue_status_request(&mut book).as_deref(), Some("1"));
    replace(&mut book, "C2", "C1", 80, 11);
    assert_eq!(venue_status_request(&mut book).as_deref(), Some("1"));
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert_eq!(venue_status_request(&mut book).as_deref(), Some("1-1"));
    run(&mut book, venue_report("1-1", Status::Canceled, 0, 0));
    assert_eq!(venue_status_request(&mut book), None);
}

/// The venue's answer is a report like any other: one showing a fill the gateway missed reaches
/// the client, one showing nothing new doesn't, as the client didn't ask.
#[test]
fn a_status_answer_reaches_the_client_only_with_news() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    venue_status_request(&mut book);
    assert!(run(&mut book, venue_report_as("1", ExecType::Status, Status::New, 0, 0)).is_empty());
    let out = run(&mut book, venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 0, 40));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.last_qty, report.cl_ord_id.as_str()), (ExecType::Trade, 40.into(), "C1"));
}

/// A venue that doesn't know the order answers Status, Rejected, OrdRejReason 5 (FIX 4.2 and 4.4
/// alike): an alarm, the order left as it is and the client not told. For a done order, nothing.
#[test]
fn a_status_answer_that_the_venue_doesnt_know_the_order_raises_an_alarm() {
    let unknown = || {
        let mut input = venue_report_as("1", ExecType::Status, Status::Rejected, 0, 0);
        let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
        report.ord_rej_reason = Some(5);
        input
    };
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    book.take_events().for_each(drop);
    let before = book.snapshot();
    assert!(run(&mut book, unknown()).is_empty());
    assert_eq!(book.snapshot(), before);
    let events: Vec<GatewayEvent> = book.take_events().collect();
    let [GatewayEvent::Alarm(alarm)] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!(alarm.kind, AlarmKind::UnknownAtVenue);

    run(&mut book, venue_report("1", Status::Canceled, 0, 0));
    assert!(run(&mut book, unknown()).is_empty());
    assert_eq!(book.take_events().count(), 0);
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
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true, sent: None },
    );
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
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true, sent: None },
    );
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
    let out = run(&mut book, Input::NewOrder { client: "FUNDB".into(), request: req, redelivered: false, sent: None });
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
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true, sent: None },
    );
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
    let out = run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: req, redelivered: false, sent: None });
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
    run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: market.clone(), redelivered: false, sent: None });
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
    run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: gtc, redelivered: false, sent: None });
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
        risk_price: Decimal::from(10),
        pending: pending.then(|| Pending {
            downstream: "1-2".into(),
            cl_ord_id: "C3".into(),
            qty: Decimal::from(80),
            price: Some(Decimal::from(11)),
            ord_type: OrdType::Limit,
            time_in_force: None,
            risk_price: Decimal::from(11),
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

/// A report raising CumQty is never stale: one whose status is behind the order's (PartiallyFilled
/// while a cancel is pending) still fills, and the order keeps its status.
#[test]
fn a_missed_fill_behind_a_pending_cancel_fills_and_keeps_the_status() {
    let mut book = limited(risk::Limits { credit: Some(2000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, fill_at("1", 40, 40, "10"));
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 40));
    let missed = venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 0, 60);
    let out = run(&mut book, priced(missed, "0", "10.5"));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.status), (ExecType::Trade, Status::PendingCancel));
    assert_eq!((report.last_qty, report.last_px), (20.into(), "11.5".parse().unwrap()));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PendingCancel);
    // 400 open and 630 filled: 970 more fits a credit of 2000, and no more.
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::Buy, 97, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C4", Side::Buy, 1, "10")), Err(RiskReject::Credit));
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
    let router = Router::new(vec![Rule {
        client: None,
        ex_destination: None,
        symbol: None,
        target: Target::Venue("NYSE".into()),
    }]);
    book.set_config(router, crate::risk::Config::default());
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

/// A gateway cancel's ack that raises CumQty carries a fill the gateway missed: it reaches the
/// client as a Trade.
#[test]
fn a_missed_fill_under_a_gateway_cancel_reaches_the_client() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::PartiallyFilled, 40, 40));
    cancel_all(&mut book, Scope::All);
    assert!(run(&mut book, venue_report("1-1", Status::PendingCancel, 0, 40)).is_empty());
    let out = run(&mut book, priced(venue_report("1-1", Status::PendingCancel, 0, 60), "0", "10.5"));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.cl_ord_id.as_str(), report.exec_type), ("C1", ExecType::Trade));
    assert_eq!((report.last_qty, report.last_px), (20.into(), "11.5".parse().unwrap()));
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

fn block(book: &mut Book, scope: Scope) {
    assert!(run(book, Input::Block { scope }).is_empty());
}

fn unblock(book: &mut Book, scope: Scope) {
    assert!(run(book, Input::Unblock { scope }).is_empty());
}

/// Whether `out` is the book's reject of a new order from a blocked client.
fn blocked_reject(out: &[Output]) -> bool {
    match out {
        [Output::Report { report, .. }] => {
            report.reject == Some(RejectReason::Blocked) && report.text.as_deref() == Some("blocked by an operator")
        }
        _ => false,
    }
}

#[test]
fn a_blocked_clients_new_orders_are_rejected_and_its_cancels_pass() {
    let mut book = book();
    new_order(&mut book, "C1");
    block(&mut book, Scope::Session("FUNDA".into()));
    let out = new_order(&mut book, "C2");
    assert!(blocked_reject(&out), "{out:?}");
    let [Output::Report { report, .. }] = out.as_slice() else { unreachable!() };
    assert_eq!((report.exec_type, report.order_id.as_deref()), (ExecType::Rejected, None));
    // Not booked, so not a duplicate later; other clients carry on.
    assert!(matches!(run(&mut book, order_from(&"FUNDB".into(), "C1")).as_slice(), [Output::NewOrder { .. }]));
    assert_eq!(cancels(&cancel(&mut book, "C3", "C1")), ["1-1"]);
}

/// While the database is behind, new orders are rejected and not booked; what takes orders off
/// the market or records a venue's report passes: a cancel, a replace, a fill.
#[test]
fn an_unavailable_gateway_rejects_new_orders_and_passes_the_rest() {
    let mut book = book();
    new_order(&mut book, "C1");
    new_order(&mut book, "C2");
    book.set_unavailable(true);
    let out = new_order(&mut book, "C3");
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.order_id.as_deref()), (ExecType::Rejected, None));
    assert_eq!(report.reject, Some(RejectReason::GatewayUnavailable));
    assert_eq!(report.text.as_deref(), Some(GATEWAY_UNAVAILABLE));
    assert_eq!(cancels(&cancel(&mut book, "C4", "C1")), ["1-1"]);
    assert!(matches!(replace(&mut book, "C5", "C2", 50, 10).as_slice(), [Output::Replace { .. }]));
    let fill = run(&mut book, venue_report("1", Status::PartiallyFilled, 10, 10));
    assert!(matches!(fill.as_slice(), [Output::Report { .. }]), "{fill:?}");
    book.set_unavailable(false);
    assert!(matches!(new_order(&mut book, "C3").as_slice(), [Output::NewOrder { .. }]));
}

#[test]
fn a_blocked_client_may_replace_only_without_raising_the_quantity() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    block(&mut book, Scope::All);
    let raised = (Some("1"), CancelKind::Replace, Status::New, CancelRejectReason::Other);
    let out = replace(&mut book, "C2", "C1", 101, 10);
    assert_eq!(cancel_reject(&out), raised);
    let [Output::CancelReject { text, .. }] = out.as_slice() else { unreachable!() };
    assert_eq!(text, "blocked: a replace may not raise the quantity");
    // The same quantity at another price, and less, pass.
    assert!(matches!(replace(&mut book, "C3", "C1", 100, 11).as_slice(), [Output::Replace { .. }]));
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert!(matches!(replace(&mut book, "C4", "C3", 50, 11).as_slice(), [Output::Replace { .. }]));
}

#[test]
fn a_comp_id_block_covers_its_sessions_including_new_ones() {
    let mut book = book();
    block(&mut book, Scope::CompId("FUNDA".into()));
    // A session the book has never seen.
    assert!(blocked_reject(&run(&mut book, order_from(&desk("LATER"), "C1"))));
    assert!(blocked_reject(&new_order(&mut book, "C1")));
    assert!(matches!(run(&mut book, order_from(&"FUNDB".into(), "C1")).as_slice(), [Output::NewOrder { .. }]));
}

#[test]
fn unblocking_lifts_exactly_its_scope() {
    let mut book = book();
    let session = Scope::Session(desk("DESK1"));
    for scope in [session.clone(), Scope::CompId("FUNDA".into()), Scope::All, Scope::All] {
        block(&mut book, scope);
    }
    assert_eq!(book.blocked(), [session.clone(), Scope::CompId("FUNDA".into()), Scope::All]);
    unblock(&mut book, session.clone());
    assert!(blocked_reject(&run(&mut book, order_from(&desk("DESK1"), "C1"))), "the CompID's block stands");
    unblock(&mut book, Scope::CompId("FUNDA".into()));
    assert!(blocked_reject(&run(&mut book, order_from(&desk("DESK1"), "C1"))), "everyone's block stands");
    unblock(&mut book, Scope::All);
    assert!(book.blocked().is_empty());
    assert!(matches!(run(&mut book, order_from(&desk("DESK1"), "C1")).as_slice(), [Output::NewOrder { .. }]));
}

#[test]
fn a_blocked_clients_redelivered_order_already_booked_gets_its_state() {
    let mut book = book();
    new_order(&mut book, "C1");
    block(&mut book, Scope::All);
    let out = run(
        &mut book,
        Input::NewOrder { client: "FUNDA".into(), request: request("C1"), redelivered: true, sent: None },
    );
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.reject, report.order_id.as_deref()), (ExecType::Status, None, Some("1")));
}

#[test]
fn a_comp_id_block_refuses_a_replace_raising_the_quantity() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    block(&mut book, Scope::CompId("FUNDA".into()));
    let raised = (Some("1"), CancelKind::Replace, Status::New, CancelRejectReason::Other);
    assert_eq!(cancel_reject(&replace(&mut book, "C2", "C1", 101, 10)), raised);
}

#[test]
fn only_a_gateway_cancel_left_unsent_says_a_send_is_waiting() {
    let mut book = book();
    new_order(&mut book, "C1");
    new_order(&mut book, "C2");
    run(&mut book, send_failed("1"));
    assert!(!book.take_cancel_unsent(), "a client's order");
    cancel_all(&mut book, Scope::All);
    run(&mut book, send_failed("2-1"));
    assert!(book.take_cancel_unsent());
    assert!(!book.take_cancel_unsent(), "taken");
}

#[test]
fn a_gateway_cancels_refused_sends_are_counted_for_the_logs() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel_all(&mut book, Scope::All);
    run(&mut book, send_failed("1-1"));
    run(&mut book, venue_up("NASDAQ"));
    run(&mut book, send_failed("1-2"));
    assert_eq!(book.cancelling()[0].1.refused_sends, 2);
}

/// A book whose clients all have `limits`, routing XNAS to NASDAQ and XNYS to NYSE.
fn limited(limits: risk::Limits) -> Book {
    let venue = |ex_destination: &str, venue: &str| Rule {
        client: None,
        ex_destination: Some(ex_destination.into()),
        symbol: None,
        target: Target::Venue(venue.into()),
    };
    let mut book = Book::new(Router::new(Vec::new()));
    let router = Router::new(vec![venue("XNAS", "NASDAQ"), venue("XNYS", "NYSE")]);
    book.set_config(router, risk::Config { defaults: limits, by_comp_id: HashMap::new() });
    book
}

fn limit_order(cl_ord_id: &str, side: Side, qty: i64, price: &str) -> OrderRequest {
    OrderRequest { side, qty: Decimal::from(qty), price: Some(price.parse().unwrap()), ..request(cl_ord_id) }
}

fn market_order(cl_ord_id: &str, side: Side, qty: i64) -> OrderRequest {
    OrderRequest { side, ord_type: OrdType::Market, price: None, ..limit_order(cl_ord_id, side, qty, "0") }
}

/// Sends `request` from FUNDA: `Ok` if it went to a venue, else the risk check it failed, whose
/// text the reject carries.
fn risk_checked(book: &mut Book, request: OrderRequest) -> Result<(), RiskReject> {
    risk_checked_from(book, &"FUNDA".into(), request)
}

fn risk_checked_from(book: &mut Book, client: &ClientId, request: OrderRequest) -> Result<(), RiskReject> {
    let out = run(book, Input::NewOrder { client: client.clone(), request, redelivered: false, sent: None });
    match out.as_slice() {
        [Output::NewOrder { .. }] => Ok(()),
        [Output::Report { report, .. }] => {
            let Some(RejectReason::Risk(reject)) = report.reject else { panic!("{out:?}") };
            assert_eq!(report.text.as_deref(), Some(reject.text()));
            assert_eq!((report.exec_type, report.order_id.as_ref()), (ExecType::Rejected, None));
            Err(reject)
        }
        _ => panic!("{out:?}"),
    }
}

/// A fill of `last_qty` at `price` on order `downstream`, taking it to `cum_qty`: the symbol's
/// reference price becomes `price`.
fn fill_at(downstream: &str, last_qty: i64, cum_qty: i64, price: &str) -> Input {
    let mut input = venue_report(downstream, Status::PartiallyFilled, last_qty, cum_qty);
    let Input::VenueReport { report, .. } = &mut input else { unreachable!() };
    report.last_px = price.parse().unwrap();
    input
}

#[test]
fn max_order_qty_refuses_a_larger_order() {
    let mut book = limited(risk::Limits { max_order_qty: Some(100.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 101, "10")), Err(RiskReject::OrderQty));
}

#[test]
fn max_notional_refuses_quantity_times_price_over_it() {
    let mut book = limited(risk::Limits { max_notional: Some(1000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Sell, 100, "10.01")), Err(RiskReject::Notional));
}

#[test]
fn symbols_refuses_any_other_symbol() {
    let mut book = limited(risk::Limits { symbols: Some(vec!["AAPL".into()]), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    let msft = OrderRequest { symbol: "MSFT".into(), ..limit_order("C2", Side::Buy, 100, "10") };
    assert_eq!(risk_checked(&mut book, msft), Err(RiskReject::Symbol));
}

/// The band is around the last fill on the symbol; with none, any price passes.
#[test]
fn price_band_refuses_a_price_too_far_from_the_last_fill_and_is_skipped_without_one() {
    let mut book = limited(risk::Limits { price_band: Some(5.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "1000")), Ok(()));
    run(&mut book, fill_at("1", 10, 10, "10"));
    for (price, expected) in [("10.5", Ok(())), ("10.51", Err(RiskReject::PriceBand)), ("9.5", Ok(()))] {
        assert_eq!(risk_checked(&mut book, limit_order(price, Side::Buy, 1, price)), expected, "{price}");
    }
    assert_eq!(risk_checked(&mut book, limit_order("C9", Side::Sell, 1, "9.49")), Err(RiskReject::PriceBand));
}

/// A Market order's notional is at the symbol's last fill; with none, it's refused where a
/// notional or credit limit needs one, and passes where none does.
#[test]
fn a_market_orders_notional_is_at_the_reference_price() {
    let mut book = limited(risk::Limits { max_notional: Some(1000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, market_order("C1", Side::Buy, 10)), Err(RiskReject::NoReferencePrice));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 10, "10")), Ok(()));
    run(&mut book, fill_at("1", 10, 10, "10"));
    assert_eq!(risk_checked(&mut book, market_order("C3", Side::Buy, 100)), Ok(()));
    assert_eq!(risk_checked(&mut book, market_order("C4", Side::Buy, 101)), Err(RiskReject::Notional));

    let mut credit = limited(risk::Limits { credit: Some(1000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut credit, market_order("C1", Side::Buy, 10)), Err(RiskReject::NoReferencePrice));
    let mut unlimited = limited(risk::Limits { max_order_qty: Some(100.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut unlimited, market_order("C1", Side::Buy, 10)), Ok(()));
}

/// Credit counts open orders' leaves and fills; a done order's leaves come off it.
#[test]
fn credit_counts_open_and_filled_and_frees_what_is_done() {
    let mut book = limited(risk::Limits { credit: Some(2000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, fill_at("1", 100, 100, "10"));
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PartiallyFilled);
    run(&mut book, venue_report("1", Status::Filled, 0, 100));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 100, "10")), Ok(()));
    // 1000 filled and 1000 open: full.
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::Sell, 1, "10")), Err(RiskReject::Credit));
    cancel(&mut book, "C4", "C2");
    run(&mut book, venue_report("2-1", Status::Canceled, 0, 0));
    assert_eq!(risk_checked(&mut book, limit_order("C5", Side::Buy, 100, "10")), Ok(()));
    // The venue rejecting it frees it too.
    run(&mut book, venue_report("3", Status::Rejected, 0, 0));
    assert_eq!(risk_checked(&mut book, limit_order("C6", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, send_failed("4"));
    assert_eq!(risk_checked(&mut book, limit_order("C7", Side::Buy, 100, "10")), Ok(()));
}

/// Self-trade prevention looks at the session's own resting orders on the other side, same
/// symbol and venue: another venue's, or another session's, don't count.
#[test]
fn self_trade_refuses_crossing_the_sessions_own_order_on_the_same_venue() {
    let mut book = limited(risk::Limits { self_trade: true, ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Sell, 100, "10")), Err(RiskReject::SelfTrade));
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::SellShort, 100, "9")), Err(RiskReject::SelfTrade));
    assert_eq!(risk_checked(&mut book, market_order("C4", Side::Sell, 100)), Err(RiskReject::SelfTrade));
    assert_eq!(risk_checked(&mut book, limit_order("C5", Side::Sell, 100, "10.01")), Ok(()));
    // Resting at 10.01, it's crossed by a buy at that price.
    assert_eq!(risk_checked(&mut book, limit_order("C6", Side::Buy, 100, "10.01")), Err(RiskReject::SelfTrade));
    let nyse = OrderRequest { ex_destination: Some("XNYS".into()), ..limit_order("C7", Side::Sell, 100, "10") };
    assert_eq!(risk_checked(&mut book, nyse), Ok(()));
    let other = limit_order("C1", Side::Sell, 100, "10");
    assert_eq!(risk_checked_from(&mut book, &desk("DESK2"), other), Ok(()));
    // Once the buy is done, nothing rests against a sell at 10.
    cancel(&mut book, "C8", "C1");
    run(&mut book, venue_report("1-1", Status::Canceled, 0, 0));
    assert_eq!(risk_checked(&mut book, limit_order("C9", Side::Sell, 100, "10")), Ok(()));
}

#[test]
fn self_trade_off_lets_crossing_orders_through() {
    let mut book = limited(risk::Limits::default());
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Sell, 100, "10")), Ok(()));
}

/// A replace is checked on its new terms: one raising the quantity past the limit is refused with
/// the limit's text, and the order keeps its terms.
#[test]
fn a_replace_raising_the_quantity_past_a_limit_is_refused() {
    let mut book = limited(risk::Limits { max_order_qty: Some(100.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    let out = replace(&mut book, "C2", "C1", 101, 10);
    assert_eq!(cancel_reject(&out), (Some("1"), CancelKind::Replace, Status::PendingNew, CancelRejectReason::Other));
    let [Output::CancelReject { text, .. }] = out.as_slice() else { unreachable!() };
    assert_eq!(text, RiskReject::OrderQty.text());
    assert!(book.order(OrderId(1)).unwrap().pending.is_none());
    assert!(matches!(replace(&mut book, "C3", "C1", 80, 10).as_slice(), [Output::Replace { .. }]));
}

/// Credit is checked on a replace only when it raises the order's notional, so a client over its
/// credit can still take risk off. A pending replace counts at its terms or the order's, the more.
#[test]
fn a_replace_is_checked_against_credit_only_if_it_adds_to_it() {
    let mut book = limited(risk::Limits { credit: Some(1000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    let out = replace(&mut book, "C2", "C1", 100, 11);
    let [Output::CancelReject { text, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(text, RiskReject::Credit.text());
    assert!(matches!(replace(&mut book, "C3", "C1", 50, 10).as_slice(), [Output::Replace { .. }]));
    // Pending, the order still counts at 1000: the venue may refuse the replace.
    assert_eq!(risk_checked(&mut book, limit_order("C4", Side::Buy, 1, "10")), Err(RiskReject::Credit));
    run(&mut book, venue_report("1-1", Status::New, 0, 0));
    assert_eq!(risk_checked(&mut book, limit_order("C5", Side::Buy, 50, "10")), Ok(()));
}

/// New limits apply to the next order; an order booked before keeps going.
#[test]
fn replaced_limits_apply_to_the_next_order_only() {
    let mut book = limited(risk::Limits::default());
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    let router = book.router().clone();
    let limits = risk::Limits { max_order_qty: Some(50.into()), ..Default::default() };
    book.set_config(router, risk::Config { defaults: limits, by_comp_id: HashMap::new() });
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 100, "10")), Err(RiskReject::OrderQty));
    let out = run(&mut book, fill_at("1", 100, 100, "10"));
    assert!(matches!(out.as_slice(), [Output::Report { .. }]), "{out:?}");
    assert_eq!(book.order(OrderId(1)).unwrap().cum_qty, Decimal::from(100));
}

/// A CompID's limits apply to each of its sessions separately.
#[test]
fn each_session_of_a_comp_id_has_its_own_credit() {
    let mut book = limited(risk::Limits { credit: Some(1000.into()), ..Default::default() });
    let (one, two) = (desk("DESK1"), desk("DESK2"));
    assert_eq!(risk_checked_from(&mut book, &one, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    let more = limit_order("C2", Side::Buy, 1, "10");
    assert_eq!(risk_checked_from(&mut book, &one, more), Err(RiskReject::Credit));
    assert_eq!(risk_checked_from(&mut book, &two, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
}

/// The CompID's own limits apply to it, the defaults to every other.
#[test]
fn a_comp_ids_own_limits_apply_to_it_only() {
    let mut book = limited(risk::Limits::default());
    let router = book.router().clone();
    let funda = risk::Limits { max_order_qty: Some(10.into()), ..Default::default() };
    book.set_config(
        router,
        risk::Config { defaults: risk::Limits::default(), by_comp_id: HashMap::from([("FUNDA".into(), funda)]) },
    );
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Err(RiskReject::OrderQty));
    assert_eq!(risk_checked_from(&mut book, &"FUNDB".into(), limit_order("C1", Side::Buy, 100, "10")), Ok(()));
}

/// A replace that adds no risk (the same price, less or the same quantity) passes whatever the
/// limits say now: here the reference has moved out of the band around the order's price.
#[test]
fn a_replace_cutting_the_quantity_at_the_same_price_skips_the_checks() {
    let mut book = limited(risk::Limits { price_band: Some(5.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 100, "20")), Ok(()));
    run(&mut book, fill_at("2", 10, 10, "20"));
    assert!(matches!(replace(&mut book, "C3", "C1", 50, 10).as_slice(), [Output::Replace { .. }]));
    let out = replace(&mut book, "C4", "C2", 100, 10);
    let [Output::CancelReject { text, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(text, RiskReject::PriceBand.text());
}

/// While a replace is pending, the order rests at the more aggressive of its two prices, since
/// the venue may have taken the replace already.
#[test]
fn self_trade_counts_a_pending_replaces_price() {
    let mut book = limited(risk::Limits { self_trade: true, ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    assert!(matches!(replace(&mut book, "C2", "C1", 100, 12).as_slice(), [Output::Replace { .. }]));
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::Sell, 100, "11")), Err(RiskReject::SelfTrade));
    // Refused, the replace leaves the order at 10.
    run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New));
    assert_eq!(risk_checked(&mut book, limit_order("C4", Side::Sell, 100, "11")), Ok(()));
}

/// A fill without a usable price leaves the reference as it was.
#[test]
fn a_fill_at_zero_keeps_the_reference() {
    let mut book = limited(risk::Limits { price_band: Some(5.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, fill_at("1", 10, 10, "10"));
    run(&mut book, fill_at("1", 10, 20, "0"));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 1, "10")), Ok(()));
}

/// A fill the gateway missed counts in filled notional at the price AvgPx implies: 400 filled,
/// then 230 more (20 at 11.5), and 400 open, so 970 more fits a credit of 2000 and no more.
#[test]
fn a_missed_fill_counts_in_filled_notional() {
    let mut book = limited(risk::Limits { credit: Some(2000.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, fill_at("1", 40, 40, "10"));
    run(&mut book, priced(venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 0, 60), "0", "10.5"));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 97, "10")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::Buy, 1, "10")), Err(RiskReject::Credit));
}

/// The reference is the last LastPx: a price implied by AvgPx for a missed fill isn't one.
#[test]
fn a_missed_fills_implied_price_keeps_the_reference() {
    let mut book = limited(risk::Limits { price_band: Some(5.into()), ..Default::default() });
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, fill_at("1", 40, 40, "10"));
    run(&mut book, priced(venue_report_as("1", ExecType::Status, Status::PartiallyFilled, 0, 60), "0", "10.5"));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 1, "10.5")), Ok(()));
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::Buy, 1, "10.51")), Err(RiskReject::PriceBand));
}

/// The trading day ending `hour` o'clock on 2026-10-10.
fn day(hour: u32) -> Day {
    use chrono::TimeZone;
    Day(Utc.with_ymd_and_hms(2026, 10, 10, hour, 0, 0).unwrap())
}

fn day_started(client: &str, day: Day) -> Input {
    Input::DaysStarted { days: vec![(client.into(), day)] }
}

/// What a new order from FUNDA was answered with: the venue's order, or the reject's reason.
fn answer(out: &[Output]) -> Option<RejectReason> {
    match out {
        [Output::NewOrder { .. }] => None,
        [Output::Report { report, .. }] => report.reject,
        _ => panic!("{out:?}"),
    }
}

/// A new day drops the session's done orders, whose ClOrdIDs it may use again, and keeps its
/// live ones, whose ClOrdIDs stay taken; another session's orders stay whatever their state.
#[test]
fn a_new_day_drops_the_sessions_done_orders_and_keeps_its_live_ones() {
    let mut book = book();
    assert!(run(&mut book, day_started("FUNDA", day(1))).is_empty());
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    new_order(&mut book, "C2");
    run(&mut book, Input::NewOrder { client: "FUNDB".into(), request: request("C1"), redelivered: false, sent: None });
    run(&mut book, venue_report("3", Status::Filled, 100, 100));

    assert!(run(&mut book, day_started("FUNDA", day(2))).is_empty());
    assert!(book.order(OrderId(1)).is_none());
    assert_eq!(book.order(OrderId(2)).unwrap().status, Status::PendingNew);
    assert_eq!(book.order(OrderId(3)).unwrap().status, Status::Filled);
    assert_eq!(book.used[&ClientId::from("FUNDA")], 1);
    assert_eq!(answer(&new_order(&mut book, "C2")), Some(RejectReason::Duplicate));
    assert_eq!(answer(&new_order(&mut book, "C1")), None);
    // The venue's late report on the dropped order is for an unknown one.
    assert!(run(&mut book, venue_report("1", Status::Filled, 0, 100)).is_empty());
}

/// A new day's ClOrdID bound counts only the live orders carried into it.
#[test]
fn a_new_day_gives_back_the_cl_ord_ids_of_done_orders() {
    let mut book = book();
    book.max_cl_ord_ids = 2;
    run(&mut book, day_started("FUNDA", day(1)));
    new_order(&mut book, "C1");
    new_order(&mut book, "C2");
    run(&mut book, venue_report("1", Status::Canceled, 0, 0));
    assert_eq!(answer(&new_order(&mut book, "C3")), Some(RejectReason::TooManyOrders));
    run(&mut book, day_started("FUNDA", day(2)));
    assert_eq!(answer(&new_order(&mut book, "C3")), None);
    assert_eq!(answer(&new_order(&mut book, "C4")), Some(RejectReason::TooManyOrders));
}

/// What the session filled counts against its credit for its day only.
#[test]
fn a_new_day_starts_the_sessions_filled_notional_again() {
    let mut book = limited(risk::Limits { credit: Some(1000.into()), ..Default::default() });
    run(&mut book, day_started("FUNDA", day(1)));
    assert_eq!(risk_checked(&mut book, limit_order("C1", Side::Buy, 100, "10")), Ok(()));
    run(&mut book, fill_at("1", 100, 100, "10"));
    run(&mut book, venue_report("1", Status::Filled, 0, 100));
    assert_eq!(risk_checked(&mut book, limit_order("C2", Side::Buy, 1, "10")), Err(RiskReject::Credit));
    run(&mut book, day_started("FUNDA", day(2)));
    assert_eq!(risk_checked(&mut book, limit_order("C3", Side::Buy, 100, "10")), Ok(()));
}

/// A session's first day only records it, and a day no later than its own changes nothing.
#[test]
fn a_first_or_earlier_day_drops_nothing() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    run(&mut book, day_started("FUNDA", day(2)));
    run(&mut book, day_started("FUNDA", day(1)));
    run(&mut book, day_started("FUNDA", day(2)));
    assert_eq!(book.day(&"FUNDA".into()), Some(day(2)));
    assert!(book.order(OrderId(1)).is_some());
    assert_eq!(answer(&new_order(&mut book, "C1")), Some(RejectReason::Duplicate));
}

/// A redelivered order the book doesn't know is rejected if it was first sent before its
/// session's day began, since the book forgot that day's done orders; one sent since, or that
/// doesn't say when, is booked. One the book knows is answered with its state, as always.
#[test]
fn a_redelivered_order_from_a_previous_day_is_rejected() {
    let mut book = book();
    run(&mut book, day_started("FUNDA", day(1)));
    let sent_at = |cl_ord_id: &str, hour: Option<u32>, redelivered: bool| Input::NewOrder {
        client: "FUNDA".into(),
        request: request(cl_ord_id),
        redelivered,
        sent: hour.map(|hour| day(hour).0),
    };
    let resent = |cl_ord_id: &str, hour: Option<u32>| sent_at(cl_ord_id, hour, true);
    // The first day began whenever: nothing is from before it.
    assert_eq!(answer(&run(&mut book, resent("C1", Some(0)))), None);
    run(&mut book, day_started("FUNDA", day(2)));
    let out = run(&mut book, resent("C2", Some(0)));
    assert_eq!(answer(&out), Some(RejectReason::Duplicate));
    let [Output::Report { report, .. }] = out.as_slice() else { unreachable!() };
    assert_eq!(report.text.as_deref(), Some("ClOrdID from a previous trading day"));
    assert_eq!(answer(&run(&mut book, resent("C3", Some(1)))), None);
    assert_eq!(answer(&run(&mut book, resent("C4", None))), None);
    assert_eq!(answer(&run(&mut book, sent_at("C5", Some(0), false))), None);
    // C1 is live, so known: its state.
    let out = run(&mut book, resent("C1", Some(0)));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.exec_type, report.reject), (ExecType::Status, None));
}

/// A redelivered cancel naming an order dropped at the end of its day is refused as unknown.
#[test]
fn a_cancel_for_an_order_dropped_at_the_days_end_is_refused_as_unknown() {
    let mut book = book();
    run(&mut book, day_started("FUNDA", day(1)));
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    run(&mut book, day_started("FUNDA", day(2)));
    let Input::Cancel { client, cl_ord_id, orig_cl_ord_id, side, symbol, .. } = cancel_input("C2", "C1") else {
        unreachable!()
    };
    let resent = Input::Cancel { client, cl_ord_id, orig_cl_ord_id, side, symbol, redelivered: true };
    assert_eq!(cancel_reject(&run(&mut book, resent)).3, CancelRejectReason::UnknownOrder);
}

/// Several sessions roll in one input, each its own way; one the book hasn't seen only records
/// its day.
#[test]
fn several_sessions_roll_at_once() {
    let mut book = book();
    let from = |client: &str, cl_ord_id: &str| Input::NewOrder {
        client: client.into(),
        request: request(cl_ord_id),
        redelivered: false,
        sent: None,
    };
    let days = |clients: &[&str], day: Day| Input::DaysStarted {
        days: clients.iter().map(|client| (ClientId::from(*client), day)).collect(),
    };
    run(&mut book, days(&["FUNDA", "FUNDB"], day(1)));
    run(&mut book, from("FUNDA", "A1"));
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    run(&mut book, from("FUNDB", "B1"));
    run(&mut book, venue_report("2", Status::Canceled, 0, 0));
    run(&mut book, from("FUNDB", "B2"));
    run(&mut book, from("FUNDC", "C1"));
    run(&mut book, venue_report("4", Status::Filled, 100, 100));

    run(&mut book, days(&["FUNDA", "FUNDB", "FUNDC"], day(2)));
    assert!(book.order(OrderId(1)).is_none() && book.order(OrderId(2)).is_none());
    assert!(book.order(OrderId(3)).is_some() && book.order(OrderId(4)).is_some());
    assert!(!book.used.contains_key(&ClientId::from("FUNDA")));
    assert_eq!(book.used[&ClientId::from("FUNDB")], 1);
    assert_eq!(book.day(&"FUNDC".into()), Some(day(2)));
}

/// A live order crosses the roll with every ClOrdID it has used, a replace's and an outstanding
/// cancel's too; the cancel's answer still reaches the client, and once done, the order goes at
/// the next roll.
#[test]
fn a_live_order_with_a_replace_and_a_cancel_crosses_the_roll() {
    let mut book = book();
    run(&mut book, day_started("FUNDA", day(1)));
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 80, 11);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    cancel(&mut book, "C3", "C2");
    run(&mut book, day_started("FUNDA", day(2)));
    assert_eq!(book.used[&ClientId::from("FUNDA")], 3);
    for used in ["C1", "C2", "C3"] {
        assert_eq!(answer(&new_order(&mut book, used)), Some(RejectReason::Duplicate), "{used}");
    }
    let out = run(&mut book, venue_report("1-2", Status::Canceled, 0, 0));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!((report.cl_ord_id.as_str(), report.exec_type), ("C3", ExecType::Canceled));
    run(&mut book, day_started("FUNDA", day(3)));
    assert!(book.order(OrderId(1)).is_none());
    assert_eq!(answer(&new_order(&mut book, "C1")), None);
}

/// Changes taken once, after several inputs and a roll between them, restore the same book: the
/// roll copies what it drops first, in the day it was written in. The store keeps the dropped
/// order as history, and the restored book leaves it out as the live one did.
#[test]
fn changes_taken_across_a_roll_restore_the_same_book() {
    let mut book = book();
    let inputs = [
        day_started("FUNDA", day(1)),
        order_from(&"FUNDA".into(), "C1"),
        venue_report("1", Status::Filled, 100, 100),
        order_from(&"FUNDA".into(), "C2"),
        venue_report("2", Status::PartiallyFilled, 40, 40),
        Input::Block { scope: Scope::CompId("FUNDB".into()) },
        day_started("FUNDA", day(2)),
        order_from(&"FUNDA".into(), "C1"),
        Input::CancelAll { scope: Scope::All },
    ];
    for input in inputs {
        book.handle(input, &mut Vec::new());
    }
    let mut snapshot = Snapshot::default();
    snapshot.apply(book.take_changes());
    assert_eq!(snapshot.orders[&OrderId(1)].order.status, Status::Filled);
    assert_eq!(snapshot.orders[&OrderId(1)].day, Some(day(1)));
    let mut restored = Book::new(book.router.clone());
    restored.restore(snapshot);
    assert!(restored.order(OrderId(1)).is_none());
    assert_eq!(restored.snapshot(), book.snapshot());
    assert_eq!(restored.used, book.used);
    assert_eq!(restored.next_id, book.next_id);
}

/// A restart, each with a new prefix as the gateway's, from every change taken so far.
fn restart(snapshot: &mut Snapshot, book: &mut Book, prefix: &str) -> Book {
    snapshot.apply(book.take_changes());
    let mut restarted = Book::new(book.router.clone());
    restarted.set_prefix(prefix.into());
    restarted.restore(snapshot.clone());
    restarted
}

/// A new order after a restart never takes the OrderId of one dropped at a roll before it, whose
/// requests and ClOrdIDs the store still has: the next restart would give them to the new order.
#[test]
fn a_restart_numbers_orders_past_those_dropped_at_a_roll() {
    let mut book = book();
    let mut snapshot = Snapshot::default();
    let handle = |book: &mut Book, input| {
        let mut out = Vec::new();
        book.handle(input, &mut out);
        out
    };
    handle(&mut book, day_started("FUNDA", day(1)));
    handle(&mut book, order_from(&"FUNDA".into(), "C1"));
    handle(&mut book, order_from(&"FUNDA".into(), "C2"));
    handle(&mut book, venue_report("2", Status::Filled, 100, 100));
    handle(&mut book, day_started("FUNDA", day(2)));
    let mut book = restart(&mut snapshot, &mut book, "B");
    let out = handle(&mut book, order_from(&"FUNDB".into(), "C1"));
    assert!(matches!(out.as_slice(), [Output::NewOrder { downstream, .. }] if downstream == "B3"), "{out:?}");
    let mut book = restart(&mut snapshot, &mut book, "C");
    assert_eq!(book.order(OrderId(3)).unwrap().client, ClientId::from("FUNDB"));
    let out = handle(&mut book, cancel_input("C3", "C2"));
    assert_eq!(cancel_reject(&out).3, CancelRejectReason::UnknownOrder);
    assert_eq!(book.next_id, 4);
}

/// A done order from before its session's first day stays until the session's first roll.
#[test]
fn a_done_order_from_before_the_first_day_goes_at_the_first_roll() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    run(&mut book, day_started("FUNDA", day(1)));
    assert!(restored(&book).order(OrderId(1)).is_some());
    run(&mut book, day_started("FUNDA", day(2)));
    assert!(restored(&book).order(OrderId(1)).is_none());
}

/// Inputs that change nothing that survives a restart leave no changes; answering a known
/// order's status takes one of its ExecIDs, which does.
#[test]
fn only_inputs_changing_what_survives_leave_changes() {
    let mut book = book();
    run(&mut book, status_request("C1"));
    let mut unroutable = request("C2");
    unroutable.ex_destination = None;
    run(&mut book, Input::NewOrder { client: "FUNDA".into(), request: unroutable, redelivered: false, sent: None });
    run(&mut book, Input::Unblock { scope: Scope::All });
    run(&mut book, day_started("FUNDA", day(1)));
    run(&mut book, day_started("FUNDA", day(1)));
    for input in [status_request("C1"), Input::Unblock { scope: Scope::All }, day_started("FUNDA", day(1))] {
        book.handle(input, &mut Vec::new());
        assert_eq!(book.take_changes(), Changes::default());
    }
    new_order(&mut book, "C1");
    book.handle(status_request("C1"), &mut Vec::new());
    let changes = book.take_changes();
    assert_eq!(changes.orders.iter().map(|row| row.order.exec_seq).collect::<Vec<_>>(), [1]);
    assert_eq!(changes, Changes { orders: changes.orders.clone(), ..Changes::default() });
}

fn orphan(downstream: &str) -> Input {
    Input::Orphan { venue: "NASDAQ".into(), downstream: downstream.into(), replace: None }
}

/// An orphaned replace to `qty` at 10.
fn orphaned_replace(downstream: &str, qty: i64) -> Input {
    let replace = OrphanedReplace {
        qty: Decimal::from(qty),
        price: Some(Decimal::from(10)),
        ord_type: OrdType::Limit,
        time_in_force: None,
    };
    Input::Orphan { venue: "NASDAQ".into(), downstream: downstream.into(), replace: Some(replace) }
}

/// FUNDA's order C1, acked by the venue.
fn acked() -> Book {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    book
}

fn gateway_cancel_sent(out: &[Output]) -> (&str, &str) {
    let [Output::Cancel { downstream, orig_downstream, .. }] = out else { panic!("{out:?}") };
    (downstream, orig_downstream)
}

/// An orphaned cancel (`1-3`, past the order's one amendment) gets the order a gateway cancel,
/// waiting for the venue's logon and numbered past it, and the venue's Canceled under the
/// orphan's ID ends the order.
#[test]
fn an_orphaned_cancel_cancels_its_order_and_the_venues_answer_applies() {
    let mut book = acked();
    replace(&mut book, "C2", "C1", 200, 10);
    assert!(run(&mut book, orphan("1-3")).is_empty());
    assert_eq!(book.downstream["1-3"].origin, Origin::Gateway);
    assert!(book.take_cancel_unsent(), "the retry tick sends it if the venue is up");
    assert_eq!(gateway_cancel_sent(&run(&mut book, venue_up("NASDAQ"))), ("1-4", "1"));
    // Recorded once: a second start finds it in the store.
    assert!(run(&mut book, orphan("1-3")).is_empty());

    let out = run(&mut book, venue_report("1-3", Status::Canceled, 0, 0));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(
        (report.exec_type, report.status, report.cl_ord_id.as_str()),
        (ExecType::Canceled, Status::Canceled, "C1")
    );
    assert!(book.cancelling().is_empty());
}

/// The venue refusing the orphan leaves the gateway's own cancel in flight, neither retried nor
/// stuck; refusing that one retries as usual.
#[test]
fn a_refused_orphan_leaves_the_gateway_cancel_going() {
    let mut book = acked();
    run(&mut book, orphan("1-1"));
    assert_eq!(gateway_cancel_sent(&run(&mut book, venue_up("NASDAQ"))), ("1-2", "1"));
    let mut refused = venue_cancel_reject("NASDAQ", "1-1", Status::New);
    if let Input::VenueCancelReject { reason, .. } = &mut refused {
        *reason = Some(1);
    }
    assert!(run(&mut book, refused).is_empty());
    let [(_, cancel)] = book.cancelling().try_into().unwrap();
    assert_eq!((cancel.tries, cancel.unsent, cancel.stuck), (1, false, false));
    let out = run(&mut book, venue_cancel_reject("NASDAQ", "1-2", Status::New));
    assert_eq!(gateway_cancel_sent(&out), ("1-3", "1"));
}

/// An orphaned replace the venue accepted applies its terms, so a fill past the old quantity
/// counts, and the order keeps working: no gateway cancel. The client hears the replace as a
/// restatement.
#[test]
fn an_accepted_orphaned_replace_applies_and_the_order_keeps_working() {
    let mut book = acked();
    assert!(run(&mut book, orphaned_replace("1-1", 200)).is_empty());
    assert_eq!(book.downstream["1-1"].kind, Some(CancelKind::Replace));
    assert!(book.cancelling().is_empty());
    assert!(run(&mut book, venue_up("NASDAQ")).is_empty());
    let out = run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    assert_eq!(report_of(&out).exec_type, ExecType::Restated);
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!(
        (order.request.qty, order.downstream.as_str(), order.request.cl_ord_id.as_str()),
        (Decimal::from(200), "1-1", "C1")
    );

    let out = run(&mut book, venue_report("1-1", Status::PartiallyFilled, 150, 150));
    let [Output::Report { report, .. }] = out.as_slice() else { panic!("{out:?}") };
    assert_eq!(
        (report.exec_type, report.cum_qty, report.leaves_qty),
        (ExecType::Trade, Decimal::from(150), Decimal::from(50))
    );
    assert_eq!(book.order(OrderId(1)).unwrap().status, Status::PartiallyFilled);
}

/// An orphaned replace the venue refuses leaves the order's terms, working, and tells the client
/// nothing: it never asked.
#[test]
fn a_rejected_orphaned_replace_leaves_the_order_working() {
    let mut book = acked();
    run(&mut book, orphaned_replace("1-1", 200));
    assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::New)).is_empty());
    let order = book.order(OrderId(1)).unwrap();
    assert_eq!((order.request.qty, order.pending.is_none(), order.status), (Decimal::from(100), true, Status::New));
    assert!(book.cancelling().is_empty());
}

/// An orphaned replace of an order done since is recorded as a replace, so the venue's refusal
/// maps to the order, and nothing is cancelled or told.
#[test]
fn an_orphaned_replace_of_a_done_order_is_recorded_as_a_replace() {
    let mut book = acked();
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    assert!(run(&mut book, orphaned_replace("1-1", 200)).is_empty());
    assert_eq!(book.downstream["1-1"].kind, Some(CancelKind::Replace));
    assert!(book.cancelling().is_empty());
    assert!(run(&mut book, venue_cancel_reject("NASDAQ", "1-1", Status::Filled)).is_empty());
}

/// Not ours to cancel: an order the book doesn't hold, another venue's, or an ID that isn't an
/// amendment's.
#[test]
fn an_orphan_naming_no_order_here_changes_nothing() {
    let mut book = book();
    new_order(&mut book, "C1");
    for input in
        [orphan("7-1"), orphan("1"), Input::Orphan { venue: "NYSE".into(), downstream: "1-1".into(), replace: None }]
    {
        assert!(run(&mut book, input).is_empty());
    }
    assert!(!book.downstream.contains_key("1-1"));
    assert!(book.cancelling().is_empty());
}

/// The client's replace C2, redelivered.
fn redelivered_replace(qty: i64) -> Input {
    let mut input = replace_input("C2", "C1", qty, 10);
    if let Input::Replace { redelivered, ord_type, .. } = &mut input {
        (*redelivered, *ord_type) = (true, Some(OrdType::Limit));
    }
    input
}

fn report_of(out: &[Output]) -> &ClientReport {
    let [Output::Report { report, .. }] = out else { panic!("{out:?}") };
    report
}

/// The client's replace, redelivered after a restart, is the orphaned replace it was sent as:
/// the venue's Replaced answers it, and fills come under it.
#[test]
fn a_redelivered_replace_adopts_the_pending_orphaned_one() {
    let mut book = acked();
    run(&mut book, orphaned_replace("1-1", 200));
    assert!(run(&mut book, redelivered_replace(200)).is_empty());
    let sent = &book.downstream["1-1"];
    assert_eq!(
        (sent.origin, sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_deref()),
        (Origin::Client, "C2", Some("C1"))
    );
    let out = run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let report = report_of(&out);
    assert_eq!(
        (report.exec_type, report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()),
        (ExecType::Replaced, "C2", Some("C1"))
    );
    let out = run(&mut book, venue_report("1-1", Status::PartiallyFilled, 150, 150));
    let report = report_of(&out);
    assert_eq!(
        (report.cl_ord_id.as_str(), report.request.qty, report.leaves_qty),
        ("C2", Decimal::from(200), Decimal::from(50))
    );
    // Other terms aren't the orphan's: a replace of its own, refused while the gateway cancels.
    let mut other = acked();
    run(&mut other, orphaned_replace("1-1", 200));
    let out = run(&mut other, redelivered_replace(300));
    assert!(matches!(out.as_slice(), [Output::CancelReject { .. }]), "{out:?}");
}

/// An orphaned replace the venue accepted before the client's redelivered request arrived:
/// the client hears a restatement under C1 with the new terms, then Replaced under C2.
#[test]
fn an_orphaned_replace_accepted_is_restated_then_adopted() {
    let mut book = acked();
    run(&mut book, orphaned_replace("1-1", 200));
    let out = run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let report = report_of(&out);
    assert_eq!((report.exec_type, report.cl_ord_id.as_str()), (ExecType::Restated, "C1"));
    assert_eq!(
        (report.request.qty, report.leaves_qty, report.request.price),
        (Decimal::from(200), Decimal::from(200), Some(Decimal::from(10)))
    );
    assert!(report.text.is_some());

    let out = run(&mut book, redelivered_replace(200));
    let report = report_of(&out);
    assert_eq!(
        (report.exec_type, report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()),
        (ExecType::Replaced, "C2", Some("C1"))
    );
    let out = run(&mut book, venue_report("1-1", Status::PartiallyFilled, 150, 150));
    assert_eq!(report_of(&out).cl_ord_id, "C2");
}

/// Replaced and a fill in one report, for an orphaned replace nobody adopted: a Trade under
/// the order's ClOrdID, never the gateway's empty one.
#[test]
fn an_orphaned_replace_accepted_with_a_fill_reports_under_the_orders_cl_ord_id() {
    let mut book = acked();
    run(&mut book, orphaned_replace("1-1", 200));
    let out = run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::PartiallyFilled, 150, 150));
    let report = report_of(&out);
    assert_eq!(
        (report.exec_type, report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref()),
        (ExecType::Trade, "C1", None)
    );
}

/// Handles `input` as `run` does, without restoring a book from the changes first: the orphaned
/// replace waiting for a pending one isn't stored (a restart finds it again in the venue's store).
fn run_live(book: &mut Book, input: Input) -> Vec<Output> {
    let mut out = Vec::new();
    book.handle(input, &mut out);
    let changes = book.take_changes();
    book.mirror.apply(changes);
    out
}

/// An orphaned replace sent after the client's replace `1-1` was answered (the answer lost with
/// it) waits for that answer: Replaced for `1-1` answers C2, then the orphan is pending, and its
/// Replaced restates the order.
#[test]
fn an_orphaned_replace_waits_for_the_pending_replaces_answer() {
    let mut book = acked();
    replace(&mut book, "C2", "C1", 200, 10);
    assert!(run(&mut book, orphaned_replace("1-2", 300)).is_empty());
    assert_eq!(book.order(OrderId(1)).unwrap().pending.as_ref().unwrap().downstream, "1-1");
    assert!(book.orphan_waiting("1-2"));
    assert!(run_live(&mut book, venue_up("NASDAQ")).is_empty());

    let out = run_live(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let report = report_of(&out);
    assert_eq!(
        (report.exec_type, report.cl_ord_id.as_str(), report.request.qty),
        (ExecType::Replaced, "C2", Decimal::from(200))
    );
    assert_eq!(book.order(OrderId(1)).unwrap().pending.as_ref().unwrap().downstream, "1-2");
    assert!(!book.orphan_waiting("1-2"));
    let out = run_live(&mut book, venue_report_as("1-2", ExecType::Replaced, Status::New, 0, 0));
    let report = report_of(&out);
    assert_eq!(
        (report.exec_type, report.cl_ord_id.as_str(), report.request.qty),
        (ExecType::Restated, "C2", Decimal::from(300))
    );
    // What's stored from here restores the same book.
    assert_eq!(restored(&book).snapshot(), book.snapshot());
}

/// The client's redelivered replace taking an orphan still waiting behind the replace pending:
/// its ClOrdID is the client's at once, so a cancel can name it, and once the orphan is made
/// pending and the venue accepts it, the client hears Replaced, not a restatement.
#[test]
fn a_redelivered_replace_adopts_a_waiting_orphaned_one() {
    let mut book = acked();
    replace(&mut book, "C2", "C1", 200, 10);
    run_live(&mut book, orphaned_replace("1-2", 300));
    let mut redelivered = replace_input("C3", "C2", 300, 10);
    if let Input::Replace { redelivered, ord_type, .. } = &mut redelivered {
        (*redelivered, *ord_type) = (true, Some(OrdType::Limit));
    }
    assert!(run_live(&mut book, redelivered).is_empty());
    assert_eq!(book.index.get(&("FUNDA".into(), "C3".into())), Some(&OrderId(1)));

    run_live(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    let sent = &book.downstream["1-2"];
    assert_eq!(
        (sent.origin, sent.cl_ord_id.as_str(), sent.orig_cl_ord_id.as_deref()),
        (Origin::Client, "C3", Some("C2"))
    );
    let out = run_live(&mut book, venue_report_as("1-2", ExecType::Replaced, Status::New, 0, 0));
    let report = report_of(&out);
    assert_eq!(
        (report.exec_type, report.cl_ord_id.as_str(), report.orig_cl_ord_id.as_deref(), report.request.qty),
        (ExecType::Replaced, "C3", Some("C2"), Decimal::from(300))
    );
    let out = run_live(&mut book, cancel_input("C4", "C3"));
    assert!(matches!(out.as_slice(), [Output::Cancel { .. }]), "{out:?}");
    assert_eq!(restored(&book).snapshot(), book.snapshot());
}

fn unsent(downstream: &str) -> Input {
    Input::Unsent { venue: "NASDAQ".into(), downstream: downstream.into() }
}

fn unanswered(book: &Book) -> Vec<String> {
    book.unanswered(&"NASDAQ".into()).into_iter().map(|(downstream, _)| downstream.to_string()).collect()
}

/// What a request was sent as, `Unsent` sends again: an order, a cancel and a replace, listed by
/// `unanswered` in the order they went, each with when it was sent, which a send again moves on.
#[test]
fn an_unsent_request_goes_again_as_it_was_sent() {
    let mut book = book();
    let (then, now) = (DateTime::UNIX_EPOCH + Duration::from_secs(1), DateTime::UNIX_EPOCH + Duration::from_secs(2));
    book.set_now(then);
    let order = new_order(&mut book, "C1");
    let cancel = cancel(&mut book, "C2", "C1");
    new_order(&mut book, "C3");
    run(&mut book, venue_report("2", Status::New, 0, 0));
    let replace = replace(&mut book, "C4", "C3", 80, 11);
    assert_eq!(unanswered(&book), ["1", "1-1", "2-1"]);
    assert!(book.unanswered(&"NASDAQ".into()).iter().all(|(_, sent_at)| *sent_at == then));
    book.set_now(now);
    for (downstream, sent) in [("1", order), ("1-1", cancel), ("2-1", replace)] {
        assert_eq!(run(&mut book, unsent(downstream)), sent, "{downstream}");
        assert_eq!(book.sent(downstream).unwrap().sent_at, now);
    }
}

/// One answered since, or whose order is done, isn't sent again: an order the venue acked is
/// left alone, and a cancel of an order filled since is refused, as a failed send.
#[test]
fn an_unsent_request_answered_since_is_not_sent_again() {
    let mut book = book();
    new_order(&mut book, "C1");
    cancel(&mut book, "C2", "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    assert_eq!(unanswered(&book), ["1-1"]);
    assert!(run(&mut book, unsent("1")).is_empty());
    run(&mut book, venue_report("1", Status::Filled, 100, 100));
    assert!(unanswered(&book).is_empty());
    let out = run(&mut book, unsent("1-1"));
    assert_eq!(cancel_reject(&out).1, CancelKind::Cancel);
    assert_eq!(cancel_reject(&out).2, Status::Filled);
}

#[test]
fn scope_names_print_as_they_parse() {
    for text in ["all", "comp:FUNDA", "session:FIX.4.4:JETBRIDGE->FUNDA/DESK1"] {
        let name: ScopeName = text.parse().unwrap();
        assert_eq!(name.to_string(), text);
    }
    assert_eq!("comp:FUNDA".parse(), Ok(ScopeName::CompId("FUNDA".into())));
    // A scope prints as its name does.
    let session = desk("DESK1");
    assert_eq!(Scope::Session(session.clone()).to_string(), ScopeName::Session(session.to_string()).to_string());
    assert_eq!(Scope::CompId("FUNDA".into()).to_string(), "comp:FUNDA");
    assert_eq!(Scope::All.to_string(), "all");
}

#[test]
fn a_scope_name_without_a_known_prefix_or_a_name_is_refused() {
    for text in ["", "ALL", "comp:", "session:", "client:FUNDA", "FUNDA"] {
        assert_eq!(text.parse::<ScopeName>(), Err(BadScope(text.into())), "{text:?}");
    }
}

/// Order 1 working for DESK1, 2 done for DESK2, 3 working for FUNDB on MSFT.
fn three_orders() -> Book {
    let mut book = book();
    run(&mut book, order_from(&desk("DESK1"), "C1"));
    run(&mut book, order_from(&desk("DESK2"), "C1"));
    let mut msft = request("C1");
    msft.symbol = "MSFT".into();
    run(&mut book, Input::NewOrder { client: "FUNDB".into(), request: msft, redelivered: false, sent: None });
    run(&mut book, venue_report("2", Status::Filled, 100, 100));
    book
}

fn ids(views: &[OrderView]) -> Vec<u64> {
    views.iter().map(|view| view.id.0).collect()
}

#[test]
fn orders_are_filtered_by_each_field() {
    let book = three_orders();
    let all = OrderFilter::default();
    assert_eq!(ids(&book.orders(&all)), [1, 2, 3]);
    assert_eq!(ids(&book.orders(&OrderFilter { working: true, ..all.clone() })), [1, 3]);
    let funda = Scope::CompId("FUNDA".into());
    assert_eq!(ids(&book.orders(&OrderFilter { client: Some(funda), ..all.clone() })), [1, 2]);
    let desk2 = Scope::Session(desk("DESK2"));
    assert_eq!(ids(&book.orders(&OrderFilter { client: Some(desk2), ..all.clone() })), [2]);
    assert_eq!(ids(&book.orders(&OrderFilter { symbol: Some("MSFT".into()), ..all.clone() })), [3]);
    assert_eq!(ids(&book.orders(&OrderFilter { status: Some(Status::Filled), ..all.clone() })), [2]);
    assert!(book.orders(&OrderFilter { venue: Some("LSE".into()), ..all }).is_empty());
}

#[test]
fn orders_page_by_order_id_at_most_a_page_at_a_time() {
    let mut book = three_orders();
    let first = OrderFilter { limit: Some(2), ..OrderFilter::default() };
    assert_eq!(ids(&book.orders(&first)), [1, 2]);
    assert_eq!(ids(&book.orders(&OrderFilter { after: Some(OrderId(2)), ..first })), [3]);
    // Straight into the book: `run` restores it per input, too slow for a page's worth.
    for n in 0..MAX_ORDERS_PER_PAGE {
        let input = order_from(&desk("DESK1"), &format!("P{n}"));
        book.handle(input, &mut Vec::new());
    }
    let page = book.orders(&OrderFilter { limit: Some(usize::MAX), ..OrderFilter::default() });
    assert_eq!(page.len(), MAX_ORDERS_PER_PAGE);
    assert_eq!((page[0].id, page[MAX_ORDERS_PER_PAGE - 1].id), (OrderId(1), OrderId(1_000)));
}

#[test]
fn an_orders_view_shows_what_is_pending() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 50, 11);
    let view = &book.orders(&OrderFilter::default())[0];
    assert_eq!((view.order_id.as_str(), view.cl_ord_id.as_str(), view.qty), ("1", "C1", 100.into()));
    assert_eq!((view.pending_cancel, view.pending_replace, view.gateway_cancel), (false, true, None));
    cancel_all(&mut book, Scope::All);
    let view = &book.orders(&OrderFilter::default())[0];
    assert!(view.pending_cancel);
    assert_eq!(view.gateway_cancel, Some(GatewayCancelView { state: CancelState::InFlight, tries: 1 }));
}

#[test]
fn an_orders_detail_lists_its_requests_in_the_order_sent() {
    let mut book = book();
    new_order(&mut book, "C1");
    run(&mut book, venue_report("1", Status::New, 0, 0));
    replace(&mut book, "C2", "C1", 50, 10);
    run(&mut book, venue_report_as("1-1", ExecType::Replaced, Status::New, 0, 0));
    cancel_all(&mut book, Scope::All);
    run(&mut book, cancel_reject_because("1-2", Status::New, 1));
    let detail = book.order_detail(OrderId(1)).unwrap();
    assert_eq!(detail.order.cl_ord_id, "C2");
    assert_eq!(detail.order.gateway_cancel, Some(GatewayCancelView { state: CancelState::Stuck, tries: 1 }));
    let requests: Vec<_> = detail
        .requests
        .iter()
        .map(|request| (request.downstream.as_str(), request.kind, request.origin, request.cl_ord_id.as_deref()))
        .collect();
    assert_eq!(
        requests,
        [
            ("1", RequestKind::New, Origin::Client, Some("C1")),
            ("1-1", RequestKind::Replace, Origin::Client, Some("C2")),
            ("1-2", RequestKind::Cancel, Origin::Gateway, None),
        ]
    );
    assert!(book.order_detail(OrderId(2)).is_none());
}

#[test]
fn an_order_is_found_by_its_name_or_its_clients_cl_ord_id() {
    let mut book = book();
    book.set_prefix("A1".into());
    new_order(&mut book, "C1");
    assert_eq!(book.order_named("A11"), Some(OrderId(1)));
    for name in ["", "A1", "11", "B11", "A11x", "A199999999999999999999"] {
        assert_eq!(book.order_named(name), None, "{name:?}");
    }
    let funda = ClientId::from("FUNDA");
    assert_eq!(book.order_by_cl_ord_id(&funda, "C1"), Some(OrderId(1)));
    assert_eq!(book.order_by_cl_ord_id(&funda, "C2"), None);
    assert_eq!(book.order_by_cl_ord_id(&"FUNDB".into(), "C1"), None);
}

/// An order restored from an earlier start keeps that start's prefix, so it's no use stripping
/// this one's.
#[test]
fn an_order_from_an_earlier_start_is_found_by_its_name() {
    let mut earlier = book();
    earlier.set_prefix("OLD9".into());
    new_order(&mut earlier, "C1");
    let mut book = book();
    book.set_prefix("NEW".into());
    book.restore(earlier.snapshot());
    assert_eq!(book.order_named("OLD91"), Some(OrderId(1)));
}

#[test]
fn the_risk_view_shows_limits_notionals_references_and_blocks() {
    let mut book = book();
    let tight = risk::Limits { max_order_qty: Some(500.into()), ..risk::Limits::default() };
    let limits =
        risk::Config { defaults: risk::Limits::default(), by_comp_id: [("FUNDA".into(), tight.clone())].into() };
    book.set_config(book.router.clone(), limits);
    new_order(&mut book, "C1");
    new_order(&mut book, "C2");
    run(&mut book, venue_report("2", Status::Filled, 100, 100));
    block(&mut book, Scope::CompId("FUNDB".into()));
    let view = book.risk_view();
    assert_eq!(view.by_comp_id, BTreeMap::from([("FUNDA".into(), tight)]));
    let funda = SessionRisk { client: "FUNDA".into(), open: 1_000.into(), filled: 1_000.into() };
    assert_eq!(view.sessions, [funda]);
    assert_eq!(view.references, BTreeMap::from([("AAPL".into(), 10.into())]));
    assert_eq!(view.blocked, [Scope::CompId("FUNDB".into())]);
}

/// The JSON the API sends: snake_case names, enums as strings, decimals as exact strings.
#[test]
fn views_serialize_as_the_api_sends_them() {
    let mut book = book();
    new_order(&mut book, "C1");
    let json = serde_json::to_value(book.order_detail(OrderId(1)).unwrap()).unwrap();
    assert_eq!(json["order_id"], "1");
    assert_eq!(json["client"], "FIX.4.4:JETBRIDGE->FUNDA");
    assert_eq!(
        (&json["side"], &json["ord_type"], &json["status"]),
        (&"buy".into(), &"limit".into(), &"pending_new".into())
    );
    assert_eq!((&json["qty"], &json["price"]), (&"100".into(), &"10".into()));
    assert_eq!(json["requests"][0]["kind"], "new");
    // The API's paging cursor.
    assert_eq!(json["id"], 1);
    // A blotter's status filter reads the names the views write.
    for status in [Status::PendingNew, Status::PartiallyFilled, Status::Canceled] {
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(serde_json::from_value::<Status>(json).unwrap(), status);
    }
    let json = serde_json::to_value(book.risk_view()).unwrap();
    assert_eq!(json["sessions"][0]["open"], "1000");
    let json = serde_json::to_value(Scope::CompId("FUNDA".into())).unwrap();
    assert_eq!(json, "comp:FUNDA");
}
