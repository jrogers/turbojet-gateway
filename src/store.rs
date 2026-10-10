//! The book's database: [`Store::write`] commits the book's [`Changes`], and [`Store::load`] reads
//! back the [`Snapshot`] a restart restores. SQLite through sqlx, in SQL PostgreSQL also speaks.
//!
//! Orders are never deleted: what the book drops at a day roll stays as history, and `load` reads
//! only what [`Book::restore`](crate::book::Book::restore) keeps. Values are stored so they read
//! back exactly: decimals as their text, which keeps the scale; times as RFC 3339 text in UTC with
//! nanoseconds, one fixed form, so two equal times are equal text; enums as short codes listed
//! both ways in one list, so renaming a variant can't change what's stored. A client is its
//! session as it shows (which the gateway keeps unambiguous) and its CompID, all a
//! [`ClientId`] holds.

use std::fmt::Display;
use std::io;
use std::str::FromStr;
use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyConnection, AnyPool, Executor, Row};
use turbojet::fields::{CompactString, Decimal};

use crate::book::{
    CancelKind, Changes, ClOrdIdRow, ClientId, Day, GatewayCancel, OrdType, Order, OrderId, OrderRequest, OrderRow,
    Origin, Pending, Scope, Sent, Side, Snapshot, Status, TimeInForce, Today,
};

/// The schema this build reads and writes. A database of another is refused, not migrated.
pub const SCHEMA_VERSION: i64 = 1;

/// The book's database: see the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Store {
    pool: AnyPool,
}

impl Store {
    /// Opens the database at `url` (`sqlite://path?mode=rwc` creates the file; `sqlite::memory:`
    /// is for tests), creating its tables if it has none.
    ///
    /// # Errors
    ///
    /// The database can't be reached, refuses the statements, or has another schema version.
    pub async fn open(url: &str) -> io::Result<Self> {
        sqlx::any::install_default_drivers();
        let sqlite = url.starts_with("sqlite:");
        // ponytail: one connection, as one writer task writes and `load` runs once at start; more
        // if reads (a blotter) come to share the store. No idle timeout or lifetime, so a
        // `sqlite::memory:` database, which is its connection's, lives as long as the store.
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    if sqlite {
                        configure_sqlite(connection).await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await
            .map_err(database("connecting"))?;
        create(&pool).await?;
        Ok(Self { pool })
    }

    /// Writes `changes` in order, in one transaction: each row replaces what the store has under
    /// its key, as [`Snapshot::apply`] does, so later rows win.
    ///
    /// # Errors
    ///
    /// The database refuses a statement or the commit; nothing is written then.
    pub async fn write(&self, changes: &[Changes]) -> io::Result<()> {
        let mut transaction = self.pool.begin().await.map_err(database("starting a write"))?;
        for changes in changes {
            write_changes(&mut transaction, changes).await?;
        }
        transaction.commit().await.map_err(database("committing a write"))
    }

    /// What [`Book::restore`](crate::book::Book::restore) keeps of the store: orders live or of
    /// their client's current day, the requests sent and ClOrdIDs of those, and all of the rest.
    ///
    /// # Errors
    ///
    /// The database refuses a query, or holds a value this build can't read.
    pub async fn load(&self) -> io::Result<Snapshot> {
        // One transaction, so the orders and what names them are of the same commit.
        let mut transaction = self.pool.begin().await.map_err(database("starting a load"))?;
        let connection = &mut *transaction;
        let mut snapshot = Snapshot::default();
        for row in fetch(connection, &format!("SELECT * FROM orders WHERE id IN ({HELD})")).await? {
            let row = order_row(&Columns(&row))?;
            snapshot.orders.insert(row.order.id, row);
        }
        for row in fetch(connection, &format!("SELECT * FROM sent WHERE order_id IN ({HELD})")).await? {
            let row = Columns(&row);
            snapshot.sent.insert(row.compact("downstream")?, sent(&row)?);
        }
        for row in fetch(connection, &format!("SELECT * FROM cl_ord_ids WHERE order_id IN ({HELD})")).await? {
            let row = Columns(&row);
            let day = row.text("day")?;
            let day = if day == NO_DAY { None } else { Some(Day(parse_time("day", &day)?)) };
            let key = (row.client()?, day, row.compact("cl_ord_id")?);
            snapshot.cl_ord_ids.insert(key, row.order_id("order_id")?);
        }
        for row in fetch(connection, "SELECT * FROM days").await? {
            let row = Columns(&row);
            let today = Today { day: Day(row.time("day_end")?), began: row.optional_time("began")? };
            snapshot.days.insert(row.client()?, today);
        }
        for row in fetch(connection, "SELECT * FROM blocks ORDER BY position").await? {
            snapshot.blocked.push(scope(&Columns(&row))?);
        }
        for row in fetch(connection, "SELECT * FROM risk_references").await? {
            let row = Columns(&row);
            snapshot.references.insert(row.compact("symbol")?, row.decimal("price")?);
        }
        for row in fetch(connection, "SELECT * FROM risk_filled").await? {
            let row = Columns(&row);
            snapshot.filled.insert(row.client()?, row.decimal("notional")?);
        }
        // Over every order, history too: see `Snapshot::next_order_id`.
        let max: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM orders")
            .fetch_one(&mut *connection)
            .await
            .map_err(database("reading the last OrderId"))?;
        snapshot.next_order_id = unsigned::<u64>("id", max)? + 1;
        transaction.commit().await.map_err(database("ending a load"))?;
        Ok(snapshot)
    }
}

/// The IDs of the orders the book holds, as `held` in the book's `persist` decides: a live order;
/// a done one written in its client's current day, or before the client's first day while that
/// day is still the first (no `began`); and any order of a client with no day. That last is for
/// rows with a day too, which `held` asserts can't be: restore finds them, not the load.
const HELD: &str = "SELECT o.id FROM orders o
    LEFT JOIN days d ON d.client = o.client AND d.client_comp_id = o.client_comp_id
    WHERE o.live = 1 OR d.client IS NULL OR o.day = d.day_end OR (o.day IS NULL AND d.began IS NULL)";

/// `cl_ord_ids.day` for a client with no day yet: part of the key, and SQL's UNIQUE would let
/// NULLs repeat. Never an RFC 3339 time.
const NO_DAY: &str = "";

/// In WAL mode `synchronous = NORMAL` syncs only at checkpoints, so a commit can be lost to a
/// power cut or an OS crash; FULL syncs the log at each commit, and a committed batch, whose
/// inbound messages the gateway then lets go, survives a machine crash. `fullfsync` makes macOS's
/// fsync reach the disk, not only its cache. As Turbojet's turbojet-sql with `sync` on.
async fn configure_sqlite(connection: &mut AnyConnection) -> Result<(), sqlx::Error> {
    connection.execute("PRAGMA journal_mode = WAL").await?;
    connection.execute("PRAGMA synchronous = FULL").await?;
    connection.execute("PRAGMA fullfsync = ON").await?;
    connection.execute("PRAGMA busy_timeout = 5000").await?;
    Ok(())
}

/// The tables, by statement. Types both SQLite and PostgreSQL know: BIGINT, as PostgreSQL's
/// INTEGER is 32 bits, and BIGINT 0 or 1 for flags, which sqlx's `Any` reads from both.
const TABLES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS orders (
        id BIGINT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        client TEXT NOT NULL,
        client_comp_id TEXT NOT NULL,
        venue TEXT NOT NULL,
        day TEXT,
        live BIGINT NOT NULL,
        cl_ord_id TEXT NOT NULL,
        account TEXT,
        symbol TEXT NOT NULL,
        side TEXT NOT NULL,
        ord_type TEXT NOT NULL,
        qty TEXT NOT NULL,
        price TEXT,
        ex_destination TEXT,
        time_in_force TEXT,
        downstream TEXT NOT NULL,
        amendments BIGINT NOT NULL,
        status TEXT NOT NULL,
        cum_qty TEXT NOT NULL,
        avg_px TEXT NOT NULL,
        risk_price TEXT NOT NULL,
        exec_seq BIGINT NOT NULL,
        venue_order_id TEXT,
        cancel TEXT,
        pending_downstream TEXT,
        pending_cl_ord_id TEXT,
        pending_qty TEXT,
        pending_price TEXT,
        pending_ord_type TEXT,
        pending_time_in_force TEXT,
        pending_risk_price TEXT,
        gateway_cancel_tries BIGINT,
        gateway_cancel_unsent BIGINT,
        gateway_cancel_stuck BIGINT,
        gateway_cancel_orig_downstream TEXT,
        gateway_cancel_refused_sends BIGINT
    )",
    "CREATE TABLE IF NOT EXISTS sent (
        downstream TEXT PRIMARY KEY,
        order_id BIGINT NOT NULL,
        origin TEXT NOT NULL,
        cl_ord_id TEXT NOT NULL,
        orig_cl_ord_id TEXT,
        kind TEXT,
        answered BIGINT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS sent_order_id ON sent (order_id)",
    "CREATE TABLE IF NOT EXISTS cl_ord_ids (
        client TEXT NOT NULL,
        client_comp_id TEXT NOT NULL,
        day TEXT NOT NULL,
        cl_ord_id TEXT NOT NULL,
        order_id BIGINT NOT NULL,
        PRIMARY KEY (client, client_comp_id, day, cl_ord_id)
    )",
    "CREATE INDEX IF NOT EXISTS cl_ord_ids_order_id ON cl_ord_ids (order_id)",
    "CREATE TABLE IF NOT EXISTS days (
        client TEXT NOT NULL,
        client_comp_id TEXT NOT NULL,
        day_end TEXT NOT NULL,
        began TEXT,
        PRIMARY KEY (client, client_comp_id)
    )",
    "CREATE TABLE IF NOT EXISTS blocks (
        position BIGINT PRIMARY KEY,
        kind TEXT NOT NULL,
        client TEXT,
        client_comp_id TEXT
    )",
    "CREATE TABLE IF NOT EXISTS risk_references (
        symbol TEXT PRIMARY KEY,
        price TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS risk_filled (
        client TEXT NOT NULL,
        client_comp_id TEXT NOT NULL,
        notional TEXT NOT NULL,
        PRIMARY KEY (client, client_comp_id)
    )",
];

/// Creates the tables a new database lacks, after checking the version of one that has them: a
/// database of another version is left as it is.
async fn create(pool: &AnyPool) -> io::Result<()> {
    let mut transaction = pool.begin().await.map_err(database("starting to create the tables"))?;
    let connection = &mut *transaction;
    let version_table = "CREATE TABLE IF NOT EXISTS schema_version (
        id BIGINT PRIMARY KEY CHECK (id = 1),
        version BIGINT NOT NULL
    )";
    connection.execute(version_table).await.map_err(database("creating the schema_version table"))?;
    sqlx::query("INSERT INTO schema_version (id, version) VALUES (1, $1) ON CONFLICT (id) DO NOTHING")
        .bind(SCHEMA_VERSION)
        .execute(&mut *connection)
        .await
        .map_err(database("recording the schema version"))?;
    let version: i64 = sqlx::query_scalar("SELECT version FROM schema_version")
        .fetch_one(&mut *connection)
        .await
        .map_err(database("reading the schema version"))?;
    if version != SCHEMA_VERSION {
        return Err(io::Error::other(format!(
            "the database's schema is version {version}; this Jetbridge reads version {SCHEMA_VERSION} only"
        )));
    }
    for table in TABLES {
        connection.execute(*table).await.map_err(database("creating the tables"))?;
    }
    transaction.commit().await.map_err(database("creating the tables"))
}

const ORDER_COLUMNS: &[&str] = &[
    "id",
    "name",
    "client",
    "client_comp_id",
    "venue",
    "day",
    "live",
    "cl_ord_id",
    "account",
    "symbol",
    "side",
    "ord_type",
    "qty",
    "price",
    "ex_destination",
    "time_in_force",
    "downstream",
    "amendments",
    "status",
    "cum_qty",
    "avg_px",
    "risk_price",
    "exec_seq",
    "venue_order_id",
    "cancel",
    "pending_downstream",
    "pending_cl_ord_id",
    "pending_qty",
    "pending_price",
    "pending_ord_type",
    "pending_time_in_force",
    "pending_risk_price",
    "gateway_cancel_tries",
    "gateway_cancel_unsent",
    "gateway_cancel_stuck",
    "gateway_cancel_orig_downstream",
    "gateway_cancel_refused_sends",
];

static UPSERT_ORDER: LazyLock<String> = LazyLock::new(|| upsert("orders", &["id"], ORDER_COLUMNS));
static UPSERT_SENT: LazyLock<String> = LazyLock::new(|| {
    let columns = ["downstream", "order_id", "origin", "cl_ord_id", "orig_cl_ord_id", "kind", "answered"];
    upsert("sent", &["downstream"], &columns)
});
static UPSERT_CL_ORD_ID: LazyLock<String> = LazyLock::new(|| {
    let key = ["client", "client_comp_id", "day", "cl_ord_id"];
    upsert("cl_ord_ids", &key, &["client", "client_comp_id", "day", "cl_ord_id", "order_id"])
});
static UPSERT_DAY: LazyLock<String> =
    LazyLock::new(|| upsert("days", &["client", "client_comp_id"], &["client", "client_comp_id", "day_end", "began"]));
static UPSERT_REFERENCE: LazyLock<String> =
    LazyLock::new(|| upsert("risk_references", &["symbol"], &["symbol", "price"]));
static UPSERT_FILLED: LazyLock<String> =
    LazyLock::new(|| upsert("risk_filled", &["client", "client_comp_id"], &["client", "client_comp_id", "notional"]));

/// `INSERT ... ON CONFLICT (key) DO UPDATE` of `columns`, bound in order as `$1`, `$2`...: the
/// upsert SQLite and PostgreSQL both take.
fn upsert(table: &str, key: &[&str], columns: &[&str]) -> String {
    let values: Vec<String> = (1..=columns.len()).map(|n| format!("${n}")).collect();
    let updates: Vec<String> =
        columns.iter().filter(|column| !key.contains(column)).map(|c| format!("{c} = excluded.{c}")).collect();
    assert!(!updates.is_empty(), "{table} has a column past its key");
    format!(
        "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {}",
        columns.join(", "),
        values.join(", "),
        key.join(", "),
        updates.join(", ")
    )
}

type Query<'q> = sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>>;

async fn write_changes(connection: &mut AnyConnection, changes: &Changes) -> io::Result<()> {
    let Changes { orders, sent, cl_ord_ids, days, blocked, references, filled } = changes;
    for row in orders {
        run(connection, bind_order(sqlx::query(&UPSERT_ORDER), row)?, "writing an order").await?;
    }
    for (downstream, sent) in sent {
        let query = sqlx::query(&UPSERT_SENT)
            .bind(downstream.as_str())
            .bind(signed(sent.order.0)?)
            .bind(sent.origin.code())
            .bind(sent.cl_ord_id.as_str())
            .bind(sent.orig_cl_ord_id.as_deref())
            .bind(sent.kind.map(Code::code))
            .bind(i64::from(sent.answered));
        run(connection, query, "writing a request sent").await?;
    }
    for ClOrdIdRow { client, day, cl_ord_id, order } in cl_ord_ids {
        let day = day.map_or_else(|| NO_DAY.to_string(), |day| time_text(day.0));
        let query = bind_client(sqlx::query(&UPSERT_CL_ORD_ID), client)
            .bind(day)
            .bind(cl_ord_id.as_str())
            .bind(signed(order.0)?);
        run(connection, query, "writing a ClOrdID").await?;
    }
    for (client, today) in days {
        let query =
            bind_client(sqlx::query(&UPSERT_DAY), client).bind(time_text(today.day.0)).bind(today.began.map(time_text));
        run(connection, query, "writing a day").await?;
    }
    if let Some(blocked) = blocked {
        run(connection, sqlx::query("DELETE FROM blocks"), "clearing the blocks").await?;
        for (position, scope) in blocked.iter().enumerate() {
            let position = i64::try_from(position).expect("blocks are a handful");
            let (kind, client, comp_id) = match scope {
                Scope::Session(client) => ("session", Some(client.to_string()), Some(client.comp_id())),
                Scope::CompId(comp_id) => ("comp_id", None, Some(comp_id.as_str())),
                Scope::All => ("all", None, None),
            };
            let query =
                sqlx::query("INSERT INTO blocks (position, kind, client, client_comp_id) VALUES ($1, $2, $3, $4)")
                    .bind(position)
                    .bind(kind)
                    .bind(client)
                    .bind(comp_id);
            run(connection, query, "writing a block").await?;
        }
    }
    for (symbol, price) in references {
        let query = sqlx::query(&UPSERT_REFERENCE).bind(symbol.as_str()).bind(price.to_string());
        run(connection, query, "writing a reference price").await?;
    }
    for (client, notional) in filled {
        let query = bind_client(sqlx::query(&UPSERT_FILLED), client).bind(notional.to_string());
        run(connection, query, "writing a client's filled notional").await?;
    }
    Ok(())
}

async fn run(connection: &mut AnyConnection, query: Query<'_>, doing: &'static str) -> io::Result<()> {
    query.execute(connection).await.map(drop).map_err(database(doing))
}

async fn fetch(connection: &mut AnyConnection, sql: &str) -> io::Result<Vec<AnyRow>> {
    sqlx::query(sql).fetch_all(connection).await.map_err(database("loading the book"))
}

fn bind_client<'q>(query: Query<'q>, client: &'q ClientId) -> Query<'q> {
    query.bind(client.to_string()).bind(client.comp_id())
}

/// Binds `row` as [`ORDER_COLUMNS`] lists the columns.
fn bind_order<'q>(query: Query<'q>, row: &'q OrderRow) -> io::Result<Query<'q>> {
    let OrderRow { order, day } = row;
    let request = &order.request;
    let pending = order.pending.as_ref();
    let cancel = order.gateway_cancel.as_ref();
    let query = bind_client(query.bind(signed(order.id.0)?).bind(order.name.as_str()), &order.client)
        .bind(order.venue.as_str())
        .bind(day.map(|day| time_text(day.0)))
        .bind(i64::from(!order.status.is_done()))
        .bind(request.cl_ord_id.as_str())
        .bind(request.account.as_deref())
        .bind(request.symbol.as_str())
        .bind(request.side.code())
        .bind(request.ord_type.code())
        .bind(request.qty.to_string())
        .bind(request.price.map(|price| price.to_string()))
        .bind(request.ex_destination.as_deref())
        .bind(request.time_in_force.map(Code::code))
        .bind(order.downstream.as_str())
        .bind(i64::from(order.amendments))
        .bind(order.status.code())
        .bind(order.cum_qty.to_string())
        .bind(order.avg_px.to_string())
        .bind(order.risk_price.to_string())
        .bind(signed(order.exec_seq)?)
        .bind(order.venue_order_id.as_deref())
        .bind(order.cancel.as_deref())
        .bind(pending.map(|pending| pending.downstream.as_str()))
        .bind(pending.map(|pending| pending.cl_ord_id.as_str()))
        .bind(pending.map(|pending| pending.qty.to_string()))
        .bind(pending.and_then(|pending| pending.price).map(|price| price.to_string()))
        .bind(pending.map(|pending| pending.ord_type.code()))
        .bind(pending.and_then(|pending| pending.time_in_force).map(Code::code))
        .bind(pending.map(|pending| pending.risk_price.to_string()))
        .bind(cancel.map(|cancel| i64::from(cancel.tries)))
        .bind(cancel.map(|cancel| i64::from(cancel.unsent)))
        .bind(cancel.map(|cancel| i64::from(cancel.stuck)))
        .bind(cancel.map(|cancel| cancel.orig_downstream.as_str()))
        .bind(cancel.map(|cancel| i64::from(cancel.refused_sends)));
    Ok(query)
}

fn order_row(row: &Columns<'_>) -> io::Result<OrderRow> {
    let request = OrderRequest {
        cl_ord_id: row.compact("cl_ord_id")?,
        account: row.optional_compact("account")?,
        symbol: row.compact("symbol")?,
        side: row.code("side")?,
        ord_type: row.code("ord_type")?,
        qty: row.decimal("qty")?,
        price: row.optional_decimal("price")?,
        ex_destination: row.optional_compact("ex_destination")?,
        time_in_force: row.optional_code("time_in_force")?,
    };
    let pending = match row.optional_compact("pending_downstream")? {
        None => None,
        Some(downstream) => Some(Pending {
            downstream,
            cl_ord_id: row.compact("pending_cl_ord_id")?,
            qty: row.decimal("pending_qty")?,
            price: row.optional_decimal("pending_price")?,
            ord_type: row.code("pending_ord_type")?,
            time_in_force: row.optional_code("pending_time_in_force")?,
            risk_price: row.decimal("pending_risk_price")?,
        }),
    };
    let gateway_cancel = match row.optional_compact("gateway_cancel_orig_downstream")? {
        None => None,
        Some(orig_downstream) => Some(GatewayCancel {
            tries: unsigned("gateway_cancel_tries", row.integer("gateway_cancel_tries")?)?,
            unsent: row.flag("gateway_cancel_unsent")?,
            stuck: row.flag("gateway_cancel_stuck")?,
            orig_downstream,
            refused_sends: unsigned("gateway_cancel_refused_sends", row.integer("gateway_cancel_refused_sends")?)?,
        }),
    };
    let order = Order {
        id: row.order_id("id")?,
        name: row.compact("name")?,
        client: row.client()?,
        venue: row.compact("venue")?,
        request,
        downstream: row.compact("downstream")?,
        amendments: unsigned("amendments", row.integer("amendments")?)?,
        status: row.code("status")?,
        cum_qty: row.decimal("cum_qty")?,
        avg_px: row.decimal("avg_px")?,
        risk_price: row.decimal("risk_price")?,
        pending,
        exec_seq: unsigned("exec_seq", row.integer("exec_seq")?)?,
        venue_order_id: row.optional_compact("venue_order_id")?,
        cancel: row.optional_compact("cancel")?,
        gateway_cancel,
    };
    let day = row.optional_time("day")?.map(Day);
    Ok(OrderRow { order, day })
}

fn sent(row: &Columns<'_>) -> io::Result<Sent> {
    Ok(Sent {
        order: row.order_id("order_id")?,
        origin: row.code("origin")?,
        cl_ord_id: row.compact("cl_ord_id")?,
        orig_cl_ord_id: row.optional_compact("orig_cl_ord_id")?,
        kind: row.optional_code("kind")?,
        answered: row.flag("answered")?,
    })
}

fn scope(row: &Columns<'_>) -> io::Result<Scope> {
    match row.text("kind")?.as_str() {
        "session" => Ok(Scope::Session(row.client()?)),
        "comp_id" => Ok(Scope::CompId(row.compact("client_comp_id")?)),
        "all" => Ok(Scope::All),
        other => Err(unreadable("kind", other, "not a scope")),
    }
}

/// A row's columns, read into the book's types; an error names the column.
struct Columns<'r>(&'r AnyRow);

impl Columns<'_> {
    fn optional(&self, column: &str) -> io::Result<Option<String>> {
        self.0.try_get(column).map_err(|e| unreadable(column, "", e))
    }

    fn text(&self, column: &str) -> io::Result<String> {
        self.optional(column)?.ok_or_else(|| unreadable(column, "NULL", "required"))
    }

    fn compact(&self, column: &str) -> io::Result<CompactString> {
        self.text(column).map(CompactString::from)
    }

    fn optional_compact(&self, column: &str) -> io::Result<Option<CompactString>> {
        self.optional(column).map(|text| text.map(CompactString::from))
    }

    fn decimal(&self, column: &str) -> io::Result<Decimal> {
        parse(column, &self.text(column)?)
    }

    fn optional_decimal(&self, column: &str) -> io::Result<Option<Decimal>> {
        self.optional(column)?.map(|text| parse(column, &text)).transpose()
    }

    fn time(&self, column: &str) -> io::Result<DateTime<Utc>> {
        parse_time(column, &self.text(column)?)
    }

    fn optional_time(&self, column: &str) -> io::Result<Option<DateTime<Utc>>> {
        self.optional(column)?.map(|text| parse_time(column, &text)).transpose()
    }

    fn code<T: Code>(&self, column: &str) -> io::Result<T> {
        let text = self.text(column)?;
        T::from_code(&text).ok_or_else(|| unreadable(column, &text, "not a code this build knows"))
    }

    fn optional_code<T: Code>(&self, column: &str) -> io::Result<Option<T>> {
        let Some(text) = self.optional(column)? else { return Ok(None) };
        T::from_code(&text).map(Some).ok_or_else(|| unreadable(column, &text, "not a code this build knows"))
    }

    fn integer(&self, column: &str) -> io::Result<i64> {
        self.0.try_get(column).map_err(|e| unreadable(column, "", e))
    }

    fn flag(&self, column: &str) -> io::Result<bool> {
        match self.integer(column)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(unreadable(column, &other.to_string(), "not 0 or 1")),
        }
    }

    fn order_id(&self, column: &str) -> io::Result<OrderId> {
        Ok(OrderId(unsigned(column, self.integer(column)?)?))
    }

    /// The row's `client` and `client_comp_id`.
    fn client(&self) -> io::Result<ClientId> {
        Ok(ClientId::from_parts(self.text("client")?, self.compact("client_comp_id")?))
    }
}

/// A time, as stored: RFC 3339 in UTC to the nanosecond, one form for each time.
fn time_text(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_time(column: &str, text: &str) -> io::Result<DateTime<Utc>> {
    let time = DateTime::parse_from_rfc3339(text).map_err(|e| unreadable(column, text, e))?;
    Ok(time.with_timezone(&Utc))
}

fn parse<T: FromStr<Err: Display>>(column: &str, text: &str) -> io::Result<T> {
    text.parse().map_err(|e| unreadable(column, text, e))
}

fn unsigned<T: TryFrom<i64>>(column: &str, value: i64) -> io::Result<T> {
    T::try_from(value).map_err(|_| unreadable(column, &value.to_string(), "out of range"))
}

/// SQL's integers are signed: a `u64` past `i64::MAX` is refused, not wrapped.
fn signed(value: u64) -> io::Result<i64> {
    i64::try_from(value).map_err(|_| io::Error::other(format!("{value} is past what the database stores")))
}

fn unreadable(column: &str, value: &str, why: impl Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("the database's {column} {value:?}: {why}"))
}

fn database(doing: &'static str) -> impl Fn(sqlx::Error) -> io::Error {
    move |e| io::Error::other(format!("{doing}: {e}"))
}

/// An enum's stored codes.
trait Code: Sized + Copy {
    fn code(self) -> &'static str;
    fn from_code(code: &str) -> Option<Self>;
}

/// Implements [`Code`] from one list of variants and codes, so both ways agree.
macro_rules! codes {
    ($($type:ty { $($variant:ident = $code:literal),+ $(,)? })+) => {$(
        impl Code for $type {
            fn code(self) -> &'static str {
                match self { $(Self::$variant => $code),+ }
            }

            fn from_code(code: &str) -> Option<Self> {
                match code { $($code => Some(Self::$variant),)+ _ => None }
            }
        }
    )+};
}

codes! {
    Side { Buy = "buy", Sell = "sell", SellShort = "sell_short" }
    OrdType { Market = "market", Limit = "limit" }
    TimeInForce { Day = "day", Gtc = "gtc", Ioc = "ioc", Fok = "fok" }
    Status {
        PendingNew = "pending_new",
        New = "new",
        PartiallyFilled = "partially_filled",
        Filled = "filled",
        PendingCancel = "pending_cancel",
        PendingReplace = "pending_replace",
        Canceled = "canceled",
        Expired = "expired",
        Rejected = "rejected",
    }
    Origin { Client = "client", Gateway = "gateway" }
    CancelKind { Cancel = "cancel", Replace = "replace" }
}

#[cfg(test)]
mod tests;
