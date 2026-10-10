use std::collections::HashSet;

use chrono::TimeZone;

use super::*;
use crate::book::{Book, ExecType, Input, Output, VenueReport};
use crate::router::{Router, Rule, Target};

async fn memory() -> Store {
    Store::open("sqlite::memory:").await.unwrap()
}

/// What a store of every change in `changes` holds, its next OrderId at least 1 as the store's
/// is (an empty snapshot's is 0).
fn applied(changes: &[Changes]) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for changes in changes {
        snapshot.apply(changes.clone());
    }
    snapshot.next_order_id = snapshot.next_order_id.max(1);
    snapshot
}

/// `hour` o'clock and `nanos` on 2026-10-10.
fn time(hour: u32, nanos: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 10, hour, 0, 0).unwrap() + chrono::Duration::nanoseconds(nanos.into())
}

fn decimal(text: &str) -> Decimal {
    text.parse().unwrap()
}

/// An order with every optional field set, or every one empty. No two fields of a type share a
/// value (decimals by value, as they compare), so a swapped column fails to read back.
fn order(id: u64, client: &ClientId, full: bool) -> Order {
    let request = OrderRequest {
        cl_ord_id: format!("C{id}").into(),
        account: full.then(|| "ACC".into()),
        symbol: "AAPL".into(),
        side: Side::Buy,
        ord_type: OrdType::Limit,
        qty: decimal("100.50"),
        price: full.then(|| decimal("10.0200")),
        ex_destination: full.then(|| "XNAS".into()),
        time_in_force: full.then_some(TimeInForce::Gtc),
    };
    let pending = Pending {
        downstream: format!("{id}-2").into(),
        cl_ord_id: format!("R{id}").into(),
        qty: decimal("200"),
        price: Some(decimal("9.25")),
        ord_type: OrdType::Limit,
        time_in_force: Some(TimeInForce::Ioc),
        risk_price: decimal("9.75"),
    };
    let gateway_cancel = GatewayCancel {
        tries: 2,
        unsent: true,
        stuck: false,
        orig_downstream: format!("{id}-4").into(),
        refused_sends: 7,
    };
    Order {
        id: OrderId(id),
        name: format!("A{id}").into(),
        client: client.clone(),
        venue: "NASDAQ".into(),
        request,
        downstream: format!("{id}-1").into(),
        amendments: 3,
        status: Status::PartiallyFilled,
        cum_qty: decimal("40.000"),
        avg_px: decimal("10.005"),
        risk_price: decimal("10.01"),
        pending: full.then_some(pending),
        exec_seq: 12,
        venue_order_id: full.then(|| "V9".into()),
        cancel: full.then(|| format!("{id}-3").into()),
        gateway_cancel: full.then_some(gateway_cancel),
    }
}

const STATUSES: [Status; 9] = [
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

/// Orders 1 to 10 of `client`, between them in every status, side, OrdType and TimeInForce,
/// with and without each optional field.
fn every_order(client: &ClientId) -> Vec<OrderRow> {
    let sides = [Side::Buy, Side::Sell, Side::SellShort];
    let ord_types = [OrdType::Market, OrdType::Limit];
    let times_in_force =
        [None, Some(TimeInForce::Day), Some(TimeInForce::Gtc), Some(TimeInForce::Ioc), Some(TimeInForce::Fok)];
    let mut orders = Vec::new();
    for (n, status) in (1..).zip(STATUSES) {
        let mut order = order(n, client, n % 2 == 0);
        let index = usize::try_from(n).unwrap();
        order.status = status;
        order.request.side = sides[index % sides.len()];
        order.request.ord_type = ord_types[index % ord_types.len()];
        order.request.time_in_force = times_in_force[index % times_in_force.len()];
        if let Some(pending) = &mut order.pending
            && index % 4 == 0
        {
            pending.ord_type = OrdType::Market;
            pending.price = None;
            pending.time_in_force = None;
        }
        orders.push(OrderRow { order, day: None });
    }
    let mut stuck = order(10, client, true);
    let cancel = stuck.gateway_cancel.as_mut().unwrap();
    (cancel.stuck, cancel.unsent) = (true, false);
    orders.push(OrderRow { order: stuck, day: None });
    orders
}

/// An original order, a replace and the gateway's cancel.
fn every_sent() -> Vec<(CompactString, Sent)> {
    let original = Sent {
        order: OrderId(1),
        origin: Origin::Client,
        cl_ord_id: "C1".into(),
        orig_cl_ord_id: None,
        kind: None,
        answered: false,
    };
    let replace = Sent {
        order: OrderId(2),
        origin: Origin::Client,
        cl_ord_id: "R2".into(),
        orig_cl_ord_id: Some("C2".into()),
        kind: Some(CancelKind::Replace),
        answered: true,
    };
    let cancel = Sent {
        order: OrderId(2),
        origin: Origin::Gateway,
        cl_ord_id: "".into(),
        orig_cl_ord_id: None,
        kind: Some(CancelKind::Cancel),
        answered: false,
    };
    vec![("1".into(), original), ("2-1".into(), replace), ("2-2".into(), cancel)]
}

/// Every enum value appears in the orders, requests or blocks, and every optional field both
/// set and empty: what the store reads back is what was written, decimals' scales too.
#[tokio::test]
async fn every_row_reads_back_as_written() {
    let store = memory().await;
    // No day for FUNDA, so every one of its orders is held, done or not.
    let funda = ClientId::from("FUNDA");
    let fundb = ClientId::from("FUNDB");
    let cl_ord_ids = vec![
        ClOrdIdRow { client: funda.clone(), day: None, cl_ord_id: "C1".into(), order: OrderId(1) },
        ClOrdIdRow { client: funda.clone(), day: Some(Day(time(17, 1))), cl_ord_id: "C1".into(), order: OrderId(3) },
    ];
    let session = ClientId::new(&turbojet::SessionId::new("FIX.4.4", "JETBRIDGE", "FUNDC"));
    let changes = Changes {
        orders: every_order(&funda),
        sent: every_sent(),
        cl_ord_ids,
        days: vec![
            (fundb.clone(), Today { day: Day(time(17, 123_456_789)), began: None }),
            (session.clone(), Today { day: Day(time(17, 0)), began: Some(time(1, 999)) }),
        ],
        blocked: Some(vec![Scope::Session(session.clone()), Scope::CompId("FUNDD".into()), Scope::All]),
        references: vec![("AAPL".into(), decimal("10.50")), ("MSFT".into(), decimal("0.0001"))],
        filled: vec![(funda.clone(), decimal("1000.00")), (fundb.clone(), Decimal::ZERO)],
        undelivered: vec![
            (1, Undelivered { client: session.clone(), message: b"8=FIX.4.4\x019=5\x0135=8\x0110=000\x01".to_vec() }),
            // Any bytes, NUL and not UTF-8 too.
            (2, Undelivered { client: fundb.clone(), message: vec![0xff, 0, 1] }),
        ],
        delivered: Vec::new(),
        orphans: vec!["Z1".into()],
    };
    store.write(std::slice::from_ref(&changes)).await.unwrap();
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded, applied(&[changes]));
    // Decimals of other scales compare equal: check the text too.
    assert_eq!(loaded.references[&CompactString::from("AAPL")].to_string(), "10.50");
    assert_eq!(loaded.orders[&OrderId(1)].order.cum_qty.to_string(), "40.000");
    assert_eq!(loaded.next_order_id, 11);
}

#[test]
fn every_code_reads_back_as_its_value_and_no_two_share_one() {
    fn check<T: Code + PartialEq + std::fmt::Debug>(values: &[T]) {
        let codes: HashSet<_> = values.iter().map(|value| value.code()).collect();
        assert_eq!(codes.len(), values.len());
        for value in values {
            assert_eq!(T::from_code(value.code()), Some(*value));
        }
    }
    check(&[Side::Buy, Side::Sell, Side::SellShort]);
    check(&[OrdType::Market, OrdType::Limit]);
    check(&[TimeInForce::Day, TimeInForce::Gtc, TimeInForce::Ioc, TimeInForce::Fok]);
    check(&[Origin::Client, Origin::Gateway]);
    check(&[CancelKind::Cancel, CancelKind::Replace]);
    check(&STATUSES);
    assert_eq!(Status::from_code("Filled"), None, "codes aren't Debug names");
}

/// Rows written later replace earlier ones under the same key, within one write and across
/// writes; a list of blocks replaces the last, and no list keeps it.
#[tokio::test]
async fn later_rows_win_and_blocks_are_replaced() {
    let store = memory().await;
    let funda = ClientId::from("FUNDA");
    let row = |status, cum_qty: &str| {
        let mut order = order(1, &funda, true);
        order.status = status;
        order.cum_qty = decimal(cum_qty);
        OrderRow { order, day: None }
    };
    let first = Changes {
        orders: vec![row(Status::New, "0"), row(Status::PartiallyFilled, "10")],
        blocked: Some(vec![Scope::All, Scope::CompId("FUNDB".into())]),
        references: vec![("AAPL".into(), decimal("10"))],
        ..Changes::default()
    };
    let second = Changes {
        orders: vec![row(Status::PartiallyFilled, "20")],
        references: vec![("AAPL".into(), decimal("11"))],
        ..Changes::default()
    };
    store.write(&[first.clone(), second.clone()]).await.unwrap();
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.orders[&OrderId(1)].order.cum_qty, decimal("20"));
    assert_eq!(loaded.references[&CompactString::from("AAPL")], decimal("11"));
    assert_eq!(loaded.blocked, [Scope::All, Scope::CompId("FUNDB".into())]);
    let third = Changes {
        orders: vec![row(Status::Filled, "100")],
        blocked: Some(vec![Scope::Session(funda.clone())]),
        ..Changes::default()
    };
    store.write(std::slice::from_ref(&third)).await.unwrap();
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded, applied(&[first, second, third]));
    assert_eq!(loaded.blocked, [Scope::Session(funda)]);
    store.write(&[Changes { blocked: Some(Vec::new()), ..Changes::default() }]).await.unwrap();
    assert!(store.load().await.unwrap().blocked.is_empty());
}

/// A delivered report's row is deleted, in the same write as its insert or a later one; writing
/// a batch again, as the writer retries one, changes nothing.
#[tokio::test]
async fn delivered_reports_are_deleted_and_a_rewrite_changes_nothing() {
    let store = memory().await;
    let row = |n: u8| Undelivered { client: ClientId::from("FUNDA"), message: vec![n] };
    let first = Changes { undelivered: vec![(1, row(1)), (2, row(2)), (3, row(3))], ..Changes::default() };
    let second = Changes { delivered: vec![1], ..Changes::default() };
    store.write(&[first.clone(), second.clone()]).await.unwrap();
    let third = Changes { delivered: vec![3], ..Changes::default() };
    store.write(std::slice::from_ref(&third)).await.unwrap();
    store.write(std::slice::from_ref(&third)).await.unwrap();
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.undelivered.into_iter().collect::<Vec<_>>(), [(2, row(2))]);
    assert_eq!(store.load().await.unwrap(), applied(&[first, second, third]));
}

/// Merged, changes leave the store as writing each in turn does, with each row once: an order
/// changed twice is one row, and a report delivered within them is neither inserted nor deleted.
#[tokio::test]
async fn merged_changes_write_what_the_changes_do() {
    let funda = ClientId::from("FUNDA");
    let row = |status| {
        let mut order = order(1, &funda, true);
        order.status = status;
        OrderRow { order, day: None }
    };
    let report = |n: u8| Undelivered { client: funda.clone(), message: vec![n] };
    let cl_ord_id = |order| ClOrdIdRow { client: funda.clone(), day: None, cl_ord_id: "C1".into(), order };
    let written = [
        Changes {
            orders: vec![row(Status::New), order_row(2)],
            cl_ord_ids: vec![cl_ord_id(OrderId(1))],
            blocked: Some(vec![Scope::All]),
            references: vec![("AAPL".into(), decimal("10"))],
            undelivered: vec![(1, report(1)), (2, report(2))],
            ..Changes::default()
        },
        Changes {
            orders: vec![row(Status::Filled)],
            cl_ord_ids: vec![cl_ord_id(OrderId(2))],
            references: vec![("AAPL".into(), decimal("11"))],
            undelivered: vec![(3, report(3))],
            delivered: vec![1, 9],
            ..Changes::default()
        },
        Changes { delivered: vec![3, 4], ..Changes::default() },
        // Inserted after its delete, as a mis-ordered queue would: it stays.
        Changes { undelivered: vec![(4, report(4))], ..Changes::default() },
    ];
    let merged = Changes::merge(written.clone());
    assert_eq!(merged.orders.len(), 2);
    assert_eq!(merged.cl_ord_ids, [cl_ord_id(OrderId(2))]);
    assert_eq!(merged.references, [("AAPL".into(), decimal("11"))]);
    assert_eq!(merged.blocked, Some(vec![Scope::All]));
    assert_eq!(merged.undelivered, [(2, report(2)), (4, report(4))]);
    assert_eq!(merged.delivered, [1, 3, 9]);
    assert_eq!(applied(std::slice::from_ref(&merged)), applied(&written));

    let (one, each) = (memory().await, memory().await);
    one.write(std::slice::from_ref(&merged)).await.unwrap();
    each.write(&written).await.unwrap();
    assert_eq!(one.load().await.unwrap(), each.load().await.unwrap());
}

/// Random sequences of batches inserting and deleting a few undelivered rows, some already in the
/// store, and setting the blocks or not: applying the merge leaves what applying each does.
#[test]
fn merging_random_changes_applies_as_each_in_turn() {
    // A fixed-seed linear congruential generator: the same cases every run.
    let mut seed: u64 = 0x5eed;
    let mut next = move |below: u64| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % below
    };
    let funda = ClientId::from("FUNDA");
    let report = |n: u64| Undelivered { client: funda.clone(), message: n.to_be_bytes().to_vec() };
    for case in 0..2_000 {
        let mut before = Snapshot::default();
        for id in 0..4 {
            if next(2) == 0 {
                before.undelivered.insert(id, report(1_000 + id));
            }
        }
        let batches: Vec<Changes> = (0..next(5) + 1)
            .map(|_| {
                let mut changes = Changes::default();
                for _ in 0..next(4) {
                    let id = next(4);
                    if next(2) == 0 {
                        changes.undelivered.push((id, report(next(100))));
                    } else {
                        changes.delivered.push(id);
                    }
                }
                changes.blocked = match next(3) {
                    0 => None,
                    1 => Some(Vec::new()),
                    _ => Some(vec![Scope::All]),
                };
                changes
            })
            .collect();
        let mut each = before.clone();
        for changes in batches.clone() {
            each.apply(changes);
        }
        let mut merged = before;
        merged.apply(Changes::merge(batches.clone()));
        assert_eq!(merged, each, "case {case}: {batches:?}");
    }
}

fn order_row(id: u64) -> OrderRow {
    OrderRow { order: order(id, &ClientId::from("FUNDB"), false), day: None }
}

/// A write that fails after its statements, before its commit, leaves nothing; the same write
/// again writes it all.
#[tokio::test]
async fn a_write_failing_inside_its_transaction_leaves_nothing() {
    let store = memory().await;
    let changes = Changes { orders: every_order(&ClientId::from("FUNDA")), ..Changes::default() };
    store.hooks.failures_in_transaction.store(1, std::sync::atomic::Ordering::Release);
    assert!(store.write(std::slice::from_ref(&changes)).await.is_err());
    assert_eq!(store.load().await.unwrap(), applied(&[]));
    store.write(std::slice::from_ref(&changes)).await.unwrap();
    assert_eq!(store.load().await.unwrap(), applied(&[changes]));
}

/// An empty store's next OrderId is 1.
#[tokio::test]
async fn an_empty_store_loads_an_empty_snapshot() {
    assert_eq!(memory().await.load().await.unwrap(), applied(&[]));
}

/// A file keeps what was written across a reopen, and a database of another schema version is
/// refused and left as it was.
#[tokio::test]
async fn a_file_reopens_and_another_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("jetbridge.db").display());
    let row = OrderRow { order: order(7, &ClientId::from("FUNDA"), true), day: Some(Day(time(17, 0))) };
    let changes = Changes { orders: vec![row], ..Changes::default() };
    let store = Store::open(&url).await.unwrap();
    store.write(std::slice::from_ref(&changes)).await.unwrap();
    store.pool.close().await;
    let store = Store::open(&url).await.unwrap();
    assert_eq!(store.load().await.unwrap(), applied(std::slice::from_ref(&changes)));
    sqlx::query("UPDATE schema_version SET version = 2").execute(&store.pool).await.unwrap();
    store.pool.close().await;
    let error = Store::open(&url).await.unwrap_err().to_string();
    assert!(error.contains("schema is version 2; this Jetbridge reads version 1"), "{error}");
    let pool = AnyPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let version: i64 = sqlx::query_scalar("SELECT version FROM schema_version").fetch_one(&pool).await.unwrap();
    assert_eq!(version, 2, "left as it was");
}

/// A book driven through new orders, fills, a replace, blocks, the gateway's cancel and day rolls
/// (one dropping done orders), its changes written to the store as taken, and restarted from the
/// store midway: after each step the store loads exactly what restoring every change keeps, and
/// a book restored from either is the same book.
#[tokio::test]
async fn a_book_restored_from_the_store_is_the_book_restored_from_its_changes() {
    let store = memory().await;
    let mut written = Vec::new();
    let mut step = async |book: &mut Book, inputs: Vec<Input>| -> Vec<Output> {
        // Each input's changes taken apart and written together, as the writer will.
        let mut out = Vec::new();
        let mut batch = Vec::new();
        for input in inputs {
            book.handle(input, &mut out);
            batch.push(book.take_changes());
        }
        store.write(&batch).await.unwrap();
        written.extend(batch);
        check(&store, &written).await;
        out
    };
    let mut book = new_book("A", None);
    let funda = ClientId::from("FUNDA");
    // FUNDB never has a day: its done order stays.
    step(&mut book, vec![new_order("FUNDB", "C1"), report("A1", ExecType::Trade, Status::Filled, 100, 100)]).await;
    // FUNDA's order done before its first day: kept through that day, gone at its roll.
    step(&mut book, vec![new_order("FUNDA", "C1"), report("A2", ExecType::Rejected, Status::Rejected, 0, 0)]).await;
    step(&mut book, vec![days_started(&funda, 1)]).await;
    step(&mut book, vec![new_order("FUNDA", "C2"), report("A3", ExecType::Trade, Status::PartiallyFilled, 40, 40)])
        .await;
    let out = step(&mut book, vec![replace("C3", "C2")]).await;
    let [Output::Replace { downstream, .. }] = out.as_slice() else { panic!("{out:?}") };
    step(&mut book, vec![report(downstream, ExecType::Replaced, Status::PartiallyFilled, 0, 40)]).await;
    // Done in the first day: gone at the roll.
    step(&mut book, vec![new_order("FUNDA", "C4"), report("A4", ExecType::Trade, Status::Filled, 100, 100)]).await;
    step(&mut book, vec![block(Scope::CompId("FUNDC".into())), block(Scope::Session("FUNDB".into()))]).await;
    step(&mut book, vec![Input::Unblock { scope: Scope::CompId("FUNDC".into()) }]).await;
    let out = step(&mut book, vec![Input::CancelAll { scope: Scope::All }]).await;
    let [Output::Cancel { downstream: gateway_cancel, .. }] = out.as_slice() else { panic!("{out:?}") };
    step(&mut book, vec![days_started(&funda, 2)]).await;
    let loaded = store.load().await.unwrap();
    let ids: HashSet<u64> = loaded.orders.keys().map(|id| id.0).collect();
    assert_eq!(ids, HashSet::from([1, 3]), "the roll dropped FUNDA's done orders");
    // A restart under a new prefix, from the store.
    let mut restarted = new_book("B", Some(loaded));
    assert_eq!(restarted.snapshot(), book.snapshot());
    let out = step(&mut restarted, vec![new_order("FUNDA", "C1")]).await;
    assert!(matches!(out.as_slice(), [Output::NewOrder { downstream, .. }] if downstream == "B5"), "{out:?}");
    step(&mut restarted, vec![report(gateway_cancel, ExecType::Canceled, Status::Canceled, 0, 40)]).await;
    assert_eq!(restarted.order(OrderId(3)).unwrap().status, Status::Canceled);
    assert_eq!(store.load().await.unwrap().next_order_id, 6);
}

/// The store loads `written` as restoring it keeps, and both restore the same book.
async fn check(store: &Store, written: &[Changes]) {
    let loaded = store.load().await.unwrap();
    let mirror = applied(written);
    let from_store = new_book("X", Some(loaded.clone()));
    let from_mirror = new_book("X", Some(mirror.clone()));
    assert_eq!(from_store.snapshot(), from_mirror.snapshot());
    let mut kept = mirror;
    kept.orders.retain(|id, _| from_mirror.order(*id).is_some());
    kept.sent.retain(|_, sent| from_mirror.order(sent.order).is_some());
    kept.cl_ord_ids.retain(|_, id| from_mirror.order(*id).is_some());
    assert_eq!(loaded, kept, "the store loads what restore keeps");
}

fn new_book(prefix: &str, snapshot: Option<Snapshot>) -> Book {
    let rule = Rule { client: None, ex_destination: None, symbol: None, target: Target::Venue("NASDAQ".into()) };
    let mut book = Book::new(Router::new(vec![rule]));
    book.set_prefix(prefix.into());
    if let Some(snapshot) = snapshot {
        book.restore(snapshot);
    }
    book
}

fn new_order(client: &str, cl_ord_id: &str) -> Input {
    let request = OrderRequest {
        cl_ord_id: cl_ord_id.into(),
        account: None,
        symbol: "AAPL".into(),
        side: Side::Buy,
        ord_type: OrdType::Limit,
        qty: Decimal::from(100),
        price: Some(Decimal::from(10)),
        ex_destination: None,
        time_in_force: None,
    };
    Input::NewOrder { client: client.into(), request, redelivered: false, sent: None }
}

fn replace(cl_ord_id: &str, orig_cl_ord_id: &str) -> Input {
    Input::Replace {
        client: "FUNDA".into(),
        cl_ord_id: cl_ord_id.into(),
        orig_cl_ord_id: orig_cl_ord_id.into(),
        side: Side::Buy,
        symbol: "AAPL".into(),
        qty: Decimal::from(200),
        price: Some(Decimal::from(11)),
        ord_type: Some(OrdType::Limit),
        time_in_force: None,
        redelivered: false,
    }
}

fn report(downstream: &str, exec_type: ExecType, status: Status, last_qty: i64, cum_qty: i64) -> Input {
    let report = VenueReport {
        downstream: downstream.into(),
        exec_type,
        status: Some(status),
        last_qty: Decimal::from(last_qty),
        last_px: Decimal::from(10),
        cum_qty: Decimal::from(cum_qty),
        avg_px: Decimal::from(10),
        exec_id: "E".into(),
        text: None,
        venue_order_id: Some("V".into()),
        ord_rej_reason: None,
    };
    Input::VenueReport { venue: "NASDAQ".into(), report }
}

fn block(scope: Scope) -> Input {
    Input::Block { scope }
}

/// `client`'s trading day ending `hour` o'clock on 2026-10-10.
fn days_started(client: &ClientId, hour: u32) -> Input {
    Input::DaysStarted { days: vec![(client.clone(), Day(time(hour, 0)))] }
}

/// Loading reads orders by index, never the whole history: no step of its plan scans `orders`.
#[tokio::test]
async fn loading_never_scans_every_order() {
    let store = memory().await;
    let mut connection = store.pool.acquire().await.unwrap();
    connection.execute("CREATE TEMPORARY TABLE held (id BIGINT PRIMARY KEY)").await.unwrap();
    for query in [HELD, HELD_ORDERS, HELD_SENT, HELD_CL_ORD_IDS] {
        let plan = sqlx::query(&format!("EXPLAIN QUERY PLAN {query}")).fetch_all(&mut *connection).await.unwrap();
        let plan: Vec<String> = plan.iter().map(|row| row.try_get("detail").unwrap()).collect();
        for step in &plan {
            assert!(
                !step.starts_with("SCAN o") && !step.starts_with("SCAN s") && !step.starts_with("SCAN c"),
                "{query}: {plan:?}"
            );
        }
    }
}

/// A ClOrdID row with no day is keyed by the sentinel, not NULL: writing it again replaces it.
#[tokio::test]
async fn a_cl_ord_id_with_no_day_is_one_row() {
    let store = memory().await;
    let funda = ClientId::from("FUNDA");
    let row = |order| ClOrdIdRow { client: funda.clone(), day: None, cl_ord_id: "X".into(), order: OrderId(order) };
    let orders = vec![
        OrderRow { order: order(1, &funda, false), day: None },
        OrderRow { order: order(2, &funda, false), day: None },
    ];
    let first = Changes { orders, cl_ord_ids: vec![row(1)], ..Changes::default() };
    store.write(&[first, Changes { cl_ord_ids: vec![row(2)], ..Changes::default() }]).await.unwrap();
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT day, order_id FROM cl_ord_ids").fetch_all(&store.pool).await.unwrap();
    assert_eq!(rows, [(NO_DAY.to_string(), 2)]);
}

/// A file store commits durably: WAL, and each commit synced (`synchronous` FULL is 2).
#[tokio::test]
async fn a_file_store_syncs_each_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&format!("sqlite://{}?mode=rwc", dir.path().join("j.db").display())).await.unwrap();
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(&store.pool).await.unwrap();
    let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous").fetch_one(&store.pool).await.unwrap();
    assert_eq!((journal.as_str(), synchronous), ("wal", 2));
}

/// A missing file isn't created unless the URL asks: the error names the file and the fix.
#[tokio::test]
async fn a_missing_file_is_an_error_naming_it_and_mode_rwc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    let error = Store::open(&format!("sqlite://{}", path.display())).await.unwrap_err().to_string();
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert!(error.contains("?mode=rwc"), "{error}");
    assert!(!path.exists());
}

#[test]
fn a_urls_password_is_hidden() {
    assert_eq!(without_password("postgres://jet:secret@db:5432/j?ssl=1"), "postgres://jet:***@db:5432/j?ssl=1");
    assert_eq!(without_password("postgres://jet@db/j"), "postgres://jet@db/j");
    assert_eq!(without_password("sqlite://a/b@c.db?mode=rwc"), "sqlite://a/b@c.db?mode=rwc");
    assert_eq!(without_password("sqlite::memory:"), "sqlite::memory:");
}

/// Creating the tables says whether the database was new, which `open` logs.
#[tokio::test]
async fn creating_says_whether_the_database_was_new() {
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    assert!(create(&pool).await.unwrap());
    assert!(!create(&pool).await.unwrap());
}

/// Requests sent under downstreams the store has, of orders it no longer loads too, and orphans
/// handled, aren't unknown; the rest are, each once, in the order asked, past one query's worth of IDs.
#[tokio::test]
async fn unknown_downstreams_are_those_without_a_request_sent() {
    let store = memory().await;
    store.write(&[Changes { sent: every_sent(), ..Changes::default() }]).await.unwrap();
    let mut asked: Vec<CompactString> = (0..LOOKUP_CHUNK + 10).map(|n| format!("X{n}").into()).collect();
    asked.splice(0..0, ["2-2".into(), "9".into(), "1".into(), "9".into()]);
    asked.push("2-1".into());
    // An orphan handled is as good as known.
    store.write(&[Changes { orphans: vec!["X0".into()], ..Changes::default() }]).await.unwrap();
    let unknown = store.unknown_downstreams(&asked).await.unwrap();
    assert_eq!(unknown.len(), LOOKUP_CHUNK + 10);
    assert_eq!(unknown[..2], ["9", "X1"]);
    assert!(!unknown.iter().any(|d| ["1", "2-1", "2-2"].contains(&d.as_str())));
    assert!(store.unknown_downstreams(&[]).await.unwrap().is_empty());
}
