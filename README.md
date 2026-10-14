# Jetbridge

A FIX connectivity gateway built on [Turbojet](https://github.com/jrogers/turbojet): clients log
on to it, it logs on to venues, and it routes the clients' orders to the venues and the venues'
reports back, keeping each order's state as it goes.

> [!WARNING]
> **Early work.** Without a [database](#database), everything is kept in memory only: a restart
> forgets every order, every report not yet delivered, operators' blocks and what the risk limits
> have counted. It hasn't been run against a real venue or client. Read [CAVEATS.md](CAVEATS.md)
> before relying on it, and don't use it to trade real money without testing it thoroughly
> yourself. It is provided as is, without warranty of any kind.

## What it does

- **FIX 4.2 and 4.4 on either side**, and across versions: a FIX 4.2 client can trade on a FIX 4.4
  venue and the other way round. Every client speaks the acceptor's version; each venue speaks
  its own.
- **NewOrderSingle, OrderCancelRequest, OrderCancelReplaceRequest and OrderStatusRequest** from
  clients; ExecutionReports and OrderCancelRejects back. Market and Limit orders; Buy, Sell and
  SellShort; Day, GTC, IOC and FOK. Anything else is rejected.
- **Routing rules**, first match wins, on the client, ExDestination(100) and Symbol(55): to a
  venue, or rejected with a text of your own.
- **ClOrdID dedupe** per client: a ClOrdID used before is rejected as a duplicate, unless the
  message is marked as a possible resend, when the client gets the order's state instead.
- **Its own IDs.** Orders go to venues under the gateway's ClOrdIDs, never the client's, and
  clients see the gateway's OrderIDs and ExecIDs. Each starts with a prefix from the gateway's
  start time, so a restart doesn't reuse them: order `0EI8001` goes to its venue as `0EI8001`,
  then `0EI8001-1` for its first cancel or replace, and its reports carry ExecIDs `0EI8001.1`,
  `0EI8001.2` and so on. Every fill reaches the client as a fill (ExecType
  Trade; Partial fill or Fill on FIX 4.2), whatever the venue called it.
- **An order counts as received once its venue has it stored**, and, with a database, once its
  change to the book has committed. A client's order, cancel or replace stays in flight on the
  client's session until then. If the client's connection drops first, the client sends it again
  when it's back (after a restart too, if its session is kept in a durable store), and the
  ClOrdID dedupe answers with the order's state. Without a database, a restart forgets the book,
  so a resent order goes to the venue as a new one, possibly a second time (see CAVEATS).
- **Reports held per client.** A report for a client that's disconnected, or reading slowly, waits
  in that client's queue and goes out, in order, once it can take it.
- **Risk limits** per CompID, checked on each order and replace before it goes to a venue: order
  quantity, notional, allowed symbols, a price band around the last fill, credit and self-trade
  prevention.
- **A kill switch**, over the API or as library calls: block a client session, a CompID or
  everyone, and cancel their orders at the venues.
- **A trading day** per client session, from its schedule or a daily `day_end`: at its end, done
  orders and their ClOrdIDs go, and the session's ClOrdID count and filled notional start again.
- **Cancel on disconnect**, set per counterparty in the sessions file: a client's orders are
  cancelled when its session drops and it doesn't log back on in time, and at shutdown.
- **A database**, optional (SQLite, or PostgreSQL with `--features postgres`): the book, blocks,
  risk counts and undelivered reports survive a restart, and the restart sorts out what a crash
  left half done with the venues. Past days' orders stay readable over the API until pruned.
- **An operator API**, optional: HTTP and JSON, with local users in three roles, an audit log of
  every change, and a websocket of the book's and the sessions' changes as they happen.
  Operators can also ask a venue where an order stands; admins can edit the risk limits and set
  sequence numbers.
- **A message log**, optional: every message both sides send and receive, in files by day,
  searchable over the API.
- **Reload on SIGHUP** of the routes, the risk limits, `day_end` and the sessions file, keeping connected
  sessions the file still has.

## Building and running

```sh
cargo run --release -- --config examples/jetbridge.toml
```

This listens for clients on 127.0.0.1:9876 and connects to a venue at 127.0.0.1:9877, retrying
until one is there. Build with `--features tls` for TLS on either side, set up in the sessions
file; without it, a sessions file that asks for TLS doesn't load.

```sh
cargo run --release --features tls -- --config examples/jetbridge.toml
```

Build with `--features gzip` to compress the [message log](#message-log)'s files.

Build with `--features postgres` for a PostgreSQL [`[database]`](#database); with `tls` too for
TLS to it.

Build with `--features console` to serve the [web console](#console) at the API's address.

Jetbridge builds against Turbojet by path, so it expects a Turbojet checkout next to it
(`../turbojet`). It needs Rust 1.89 or later.

## Configuration

Two files: `jetbridge.toml`, given with `--config`, and the sessions file it names.

### jetbridge.toml

```toml
# Jetbridge: the sessions file, how orders are routed and the clients' risk limits. First match
# wins; an order no route matches is rejected.
sessions = "sessions.toml"
day_end = "17:00 America/New_York"  # when client sessions' trading days end

# FUNDA's orders for XNAS go to NASDAQ.
[[route]]
client = "FUNDA"
ex_destination = "XNAS"
venue = "NASDAQ"

[[route]]
reject = "no route for this order"

# Every client's limits. A key left out is no check.
[risk.defaults]
max_order_qty = 10000             # OrderQty(38)
max_notional = 1000000            # quantity x price
price_band = 5                    # percent either side of the symbol's last fill
self_trade = true

# FUNDA's own: the keys it sets replace the defaults', the rest are the defaults'.
[risk.client.FUNDA]
max_notional = "2500000.50"
credit = 10000000                 # open + filled today + this order

# The operator API, on loopback: off it, it needs tls = { cert, key } or behind_proxy = true.
[api]
listen = "127.0.0.1:8080"
users = "users.toml"              # paths relative to this file
audit_dir = "audit"               # a file a day; audit_retention = "400d" deletes older ones

# Every message the clients' and venues' sessions receive and send, in files by day, passwords
# masked. Left out, nothing is logged.
# [message_log]
# dir = "log"                     # relative to this file
# retention = "90d"               # files older are deleted; kept for ever if left out
# compress = true                 # gzip finished files; needs the `gzip` feature
```

- `sessions`: the sessions file, relative to this file. Read at start only; a reload rereads the
  file it named then.
- `[[route]]`, any number, tried in order for each new order; the first that matches decides. A
  route matches when every condition it sets holds, and one it leaves out matches anything:
  - `client`: the client's CompID (its SenderCompID). It matches every session of that CompID.
  - `ex_destination`: the order's ExDestination(100). A route that sets it doesn't match an order
    without one.
  - `symbol`: the order's Symbol(55).
  - and one of `venue`, the name of an `[initiator.NAME]` in the sessions file, which the order
    goes to, or `reject`, the Text(58) of the reject the order gets instead.

An order no route matches is rejected with "no route for this order". With no routes at all every
order is rejected, and routes after one with no conditions can never match; both load, with a
warning logged. Unknown keys are errors, and so is a route to a venue the sessions file has no
initiator for. Cancels and replaces go to the venue the order went to, whatever the routes say
now.
- `day_end`: when client sessions' trading days end (a schedule's period may end one sooner): see
  [Trading day](#trading-day).
- `[risk.defaults]` and `[risk.client.COMPID]`: see [Risk limits](#risk-limits).
- `[database]`: see [Database](#database).
- `[api]`, optional: the operator API, an HTTP JSON server in the gateway's process. `listen` is
  an IP address and port; `users` is a file and `audit_dir` a directory, relative to this file;
  `audit_retention`, optional, is how long the [audit log](#audit-log)'s daily files are kept;
  `tls = { cert, key }` (PEM, with the `tls` feature) serves HTTPS, and a reload takes a renewed
  certificate; other changes apply at the next start, and an `[api]` that doesn't load refuses
  the whole reload of this file. Off loopback the API refuses to start without `tls`, unless
  `behind_proxy = true` says a proxy in front terminates TLS. See [API](#api).
- `[message_log]`, optional: see [Message log](#message-log).

### sessions.toml

```toml
# Turbojet's sessions file (turbojet-config): the acceptor clients log on to, and an initiator
# per venue, named as routes name it.

[acceptor]
begin_string = "FIX.4.4"           # every client speaks this
sender_comp_id = "JETBRIDGE"
listen = "127.0.0.1:9876"

[counterparty.FUNDA]

# Jetbridge logs on to NASDAQ as JETBRIDGE.
[initiator.NASDAQ]
target_comp_id = "NASDAQ"
connect = ["127.0.0.1:9877"]
```

This is Turbojet's sessions file, read by `turbojet-config`; its keys (stores, schedules,
throttling, TLS, failover and the rest) are documented in
[turbojet-config](https://github.com/jrogers/turbojet/tree/main/crates/turbojet-config). For
Jetbridge:

- `[acceptor]` is where clients log on. It's required, and its `begin_string`, `FIX.4.2` or
  `FIX.4.4`, is every client's version. Which CompIDs may log on is turbojet-config's: those listed
  as `[counterparty.COMPID]`, or any under `unknown = "admit"`. A client is a session: sessions of
  one CompID with different SubIDs or LocationIDs each have their own ClOrdIDs, reports and risk
  counts.
- Each `[initiator.NAME]` is a venue, and `NAME` is what routes call it. A venue speaks its own
  `begin_string` (the acceptor's by default), `FIX.4.2` or `FIX.4.4`, and logs on as the
  acceptor's `sender_comp_id` unless it sets its own. A venue with another version stops the
  gateway at start; a reload that adds one skips it, with an error logged.
- Sessions are kept in the `memory` store unless the file says otherwise, so sequence numbers
  start again at each restart. A `[store.NAME]` with `kind = "disk"` keeps them. With a
  [database](#database), the acceptor and every venue must be kept in one.

## Database

With a `[database]` in `jetbridge.toml`, the book survives a restart: its orders, the ClOrdIDs each
client has used and the requests sent to venues, operators' blocks, what the risk limits count
(each session's filled notional, each symbol's last fill price), and the reports not yet delivered
to clients.

```toml
[database]
url = "sqlite:///var/lib/jetbridge/jetbridge.db?mode=rwc"
retention = "400d"
```

- `url`: a SQLite database, or a PostgreSQL one with the `postgres` feature. It's read at start
  only: a reload that finds it changed logs a warning and keeps the database in use. For SQLite,
  `?mode=rwc` creates the file if it's missing, with a warning logged that a new, empty database
  was created; without it, a missing file stops the start, so a mistyped path doesn't start an
  empty book. A relative path is from the working directory, not from this file's directory as
  `sessions` is.
- `retention`, optional: how long a done order is kept after its trading day ends, written as
  `[message_log]`'s `retention`, and at least `1d`. Left out, nothing is deleted. A reload takes a
  changed one. See Pruning, below.
- Without a `[database]`, the gateway keeps everything in memory, and a restart forgets it.

**On PostgreSQL.**

```toml
[database]
url = "postgres://jetbridge@db.internal/jetbridge?sslmode=verify-full"
```

- The database must already exist (`?mode=rwc` is SQLite's), and the user needs CREATE on its
  default schema (its `search_path`): on PostgreSQL 15 and later, `public` grants that only to the
  database's owner. The tables are created on the first start.
- The password can be in the URL, as `password=`, or in `PGPASSWORD` or `~/.pgpass`, which sqlx
  reads; in the URL it's masked in the log. Spell it `password`: sqlx logs a parameter it doesn't
  know with its value, so a misspelt `passwd=` puts the password in the log.
- TLS needs the `tls` feature: without it, `sslmode=require` doesn't connect. The default,
  `prefer` (unless `PGSSLMODE` sets another), then connects in the clear: queries, data, and the
  password itself if the server asks for it by its `password` method. `verify-full` and
  `verify-ca` check the server's certificate against the operating system's roots and any
  `sslrootcert=`.
- Each gateway holds three connections (one to write, two read-only to read past orders), within
  the server's `max_connections`.

**Orders don't wait for the database.** Orders go to venues and reports to clients without
waiting for it; the changes are committed in the background, many at a time. Each message that
changed the book counts as received by its session only once its change has committed, so a stop
before then has its sender send it again (see below for the stores that takes). A commit is
synced to disk: SQLite's by the gateway, PostgreSQL's by the server while it keeps
`synchronous_commit` on, its default.

**Sessions need a durable store.** With a database, the `[acceptor]` and every
`[initiator.NAME]` need a store that survives a restart (`kind = "disk"`, say), with its files
kept: a restart reads each venue's store to see what was sent, and a client's messages that
hadn't committed come back only through its session's stored sequence numbers. A start with a
sessions file that keeps a client's or venue's session in memory stops, saying so; so does one
that finds a venue's store has no record of its session while the database has live orders, or
orders of a client's current day, for that venue.

**What a restart does.** Before any session starts, the gateway restores the book as last
committed, then reads every request each venue's session stored and compares them with the
database:

- an order the venue's session stored but the database never recorded (the gateway stopped
  within a commit of sending it) is cancelled at the venue's next logon, logged as an error. The
  client sends its order again, and it's booked and sent afresh. An unrecorded cancel cancels its
  order; an unrecorded replace stays pending on its order, which keeps working.
- a request the database has, unanswered, that the venue's session never stored is sent at the
  venue's next logon, marked PossResend(97), unless the venue's session resets its sequence
  numbers at that logon: then it fails, and the client gets a reject.
- reports not yet delivered go to their clients again, marked PossResend(97), once they log on.
  So do reports made again from a message the venue or client sends again because its change
  hadn't committed: a fill can reach a client twice, under the same ExecID, the second time
  marked PossResend.

New IDs take a new prefix from the start time, and restored orders keep theirs. Each start waits
for the next second before taking its prefix, so it takes up to a second longer.

**Gateway unavailable.** While the oldest change not yet committed has waited over a second (the
database is down, or far behind), new orders are rejected with Text(58) "gateway unavailable"
(OrdRejReason 0 on FIX 4.2, 99 on FIX 4.4). Cancels, replaces and venue reports keep flowing,
each held until it commits, and a failing commit is retried with an error logged each time. A
session stops reading 256 messages past its oldest one waiting.

**Pruning.** With a `retention`, the gateway deletes, every hour from an hour after it starts,
the done orders whose trading day ended, and whose requests were sent, longer ago than that,
with their requests sent and ClOrdIDs, a few at a time, so the book's writes wait only briefly. It keeps what a restart needs:
live orders, each client's current trading day however old, the newest order (the next OrderID
follows it) and any order booked before its client had a trading day. While work a start found
for the venues is still pending (orphans to cancel, requests to send at a venue's logon), a run is
skipped with a warning saying what it waits for. The startup log line counts the requests in
venues' stores that belong to pruned orders as `pruned`.

What recovery doesn't cover is in [CAVEATS.md](CAVEATS.md#database-and-recovery).

## Message log

With a `[message_log]` in `jetbridge.toml`, every message the clients' and venues' sessions receive
and send is written to files in one directory, both sides in the same files, for a record of what
was said and to search over the [API](#searching-the-message-log).

```toml
[message_log]
dir = "/var/log/jetbridge/fix"
file_bytes_max = 268435456
retention = "90d"
compress = true
```

- `dir`: where the files go, relative to this file; created if missing. A log that can't be
  opened stops the start.
- `file_bytes_max`, optional: a new file is started once one reaches this many bytes; 256 MiB if
  left out.
- `retention`, optional: files older than this are deleted. A whole number and `s`, `m`, `h` or
  `d` (`90d`), at most 100 years; left out, files are kept for ever.
- `compress`, optional: gzip each file once it's finished. It needs the `gzip` feature: a build
  without it refuses `compress = true`.

Unknown keys are errors. Files are named `messages-YYYYMMDD-NNNNNN.log` (`.log.gz` once
compressed), by UTC day and a number within it: a new one starts each UTC day, when the current
one is full, and at each start. Each message is a line `<time> <in|out> <length> <session>`, the
time in UTC and the session `-` until a client's Logon names it, then the message as on the wire;
passwords are written as `*`s. Turbojet's `turbojet-log` command reads them. The log is opened at
start only: a reload that finds `[message_log]` changed keeps the one in use and warns that the
change needs a restart.

## Risk limits

Each new order a route sends to a venue, and each replace, is checked against its client's limits
first. `[risk.defaults]` sets every client's; `[risk.client.COMPID]` sets one CompID's. Every key is
optional, and one left out is no check:

| Key | Refuses an order when |
| --- | --- |
| `max_order_qty` | its OrderQty(38) is more than this |
| `max_notional` | its quantity times its price is more than this |
| `symbols` | its Symbol(55) isn't one of these, compared exactly |
| `price_band` | it's a Limit order priced more than this percentage either side of the symbol's last fill; no check before the symbol's first fill |
| `credit` | the session's open notional, plus what it has filled in its trading day, plus this order's, is more than this |
| `self_trade` | `true`: it would trade with one of the same session's live orders on the other side, same symbol and venue (a buy at or above a resting sell's price, a sell at or below a resting buy's; a Market order on either side crosses) |

Amounts are TOML integers, floats or strings, read as decimals (a string keeps every digit as
written), and must be positive; `symbols` can't be empty. Unknown keys are errors.

- **Per CompID, each session separately.** A `[risk.client.COMPID]` key replaces the defaults'
  one key by key; the keys it leaves out are the defaults'. It can't remove a numeric default or
  the defaults' `symbols`: set a large number, or list the symbols. `self_trade = false` does turn
  off the defaults'. The limits apply to each of the CompID's sessions on its own: two sessions of
  one CompID each have the full credit.
- **Prices.** A Limit order counts at its limit price. A Market order counts at the symbol's last
  fill price, on any venue and for any client. Without one, a Market order is rejected where
  `max_notional` or `credit` applies, and passes otherwise. An order's open notional is its
  unfilled quantity at that price, fixed when it's booked or replaced.
- **Credit** counts the session's live orders (with a pending replace, the larger of the old and
  new terms) and its fills, at their fill prices, in its [trading day](#trading-day).
- **Replaces** are checked on their new terms, against credit only if they raise the order's
  notional. A replace that adds no risk, at the same price (or Market again) and no more quantity,
  always passes, so a client can cut an order the limits would refuse now.
- **What the client sees.** A refused order gets a Rejected ExecutionReport whose Text(58) names
  the limit, such as "notional over the limit (risk max_notional)". Its OrdRejReason(103) is 3
  (exceeds limit) for `max_order_qty`, `max_notional` and `credit`, and 0 (broker option) on FIX
  4.2 or 99 (other) on FIX 4.4 for the rest. A refused replace gets an OrderCancelReject with the
  same text. New orders are checked in this order: a duplicate ClOrdID, blocked by an operator,
  too many ClOrdIDs, the routes (a route's reject or no route), then the limits.
- **Reloads.** A SIGHUP rereads the limits with the routes, both or neither. New limits apply from
  the next order or replace; orders already working stay, and what's been counted carries over.
  An admin can also edit them over the API, which rewrites the file and reloads: see
  [Editing the risk limits](#editing-the-risk-limits).

## Trading day

Each client session has a trading day. When it ends, the session starts the next one:

- its done orders (filled, cancelled, expired, rejected) are forgotten, and their ClOrdIDs can be
  used again;
- its live orders (GTC ones, or any still working) carry over, and their ClOrdIDs stay in use;
- its count of ClOrdIDs against the 1,000,000 a day starts again from its live orders' ClOrdIDs;
- what it has filled, for `credit`, starts again at zero.

Symbols' last fill prices are every client's, and carry over. With a database, the done orders
forgotten can still be read over the [API](#endpoints), until pruned.

A session's day ends at `day_end` in `jetbridge.toml`, or sooner, when its schedule's period
(from the sessions file) ends: whichever comes first. `day_end` is a time, `HH:MM` or
`HH:MM:SS`, and a time zone, `UTC`, an offset such as `+05:30` or an IANA name such as
`America/New_York` (daylight saving followed). The zone is UTC if left out, and `day_end` is
midnight UTC if left out. So:

- without a schedule, a session's day ends at each `day_end`;
- with a daily schedule, `daily 08:00-17:00 America/New_York` say, it ends at 17:00 New York
  time with the period, and again at `day_end` while the session is logged out, which changes
  nothing it would notice;
- with a weekly schedule, `weekly sun 17:00-fri 17:00` say, each day of the week ends at
  `day_end`, and the last when the period ends.

A possible resend (PossDupFlag or PossResend, or one Turbojet says may have been delivered
before) of an order the gateway doesn't know is rejected as a duplicate, "ClOrdID from a previous
trading day", if it was first sent (its OrigSendingTime, else its SendingTime) before the
session's day began: the gateway has forgotten that day's done orders, so can't tell whether it
took it.

The day rolls over at a session's first message after its end, before that message is handled,
or within a second by itself if the session is quiet. A SIGHUP rereads `day_end` with the routes
and limits; a new `day_end`, or a changed schedule, applies from each session's next day: the day
under way ends when it was going to.

## Kill switch

An operator can stop a client trading, over the [API](#api) or with library calls on the
running gateway (`Service::gateway()`). Each takes a `Scope`: one client session, a CompID (all
its sessions, including ones that log on later), or everyone. They're async, and with a database
they return once their change has committed.

- `cancel_all(scope)` blocks the scope and cancels its clients' live orders at their venues, in
  one step. Each client gets an unsolicited Canceled report for each order as its venue confirms
  it. A cancel the venue refuses is sent again, three tries in all; `cancelling()` lists the
  cancels still going and those given up. A cancel for a venue that is down goes when it logs
  back on; one its session couldn't queue is tried again every second while its session is
  connected.
- `block(scope)` only blocks: a blocked client's new orders are rejected ("blocked by an
  operator"). Its cancels pass, and so do replaces that don't raise the quantity, even at another
  price.
- `unblock(scope)` lifts the block of that scope exactly, returning `false` if it wasn't blocked:
  unblocking a session leaves a block of its CompID in force. `blocked()` lists the blocked
  scopes.

Each call that changes something is logged as a warning naming its scope. A blocked client's
replaces are still checked against its risk limits.

## Cancel on disconnect

A counterparty's `cancel_on_disconnect` in the sessions file turns it on for its sessions:

```toml
[counterparty.FUNDA]
cancel_on_disconnect = "disconnect"   # or "disconnect_or_logout": Logouts count too
cancel_grace = "5s"                   # how long it has to log back on; 0s by default
```

When such a session's connection ends and it doesn't log back on within the grace, the gateway
cancels its live orders at their venues, as `cancel_all` does for one session but without a
block: the client may log back on and trade. It's logged as a warning naming the session. The
client gets an unsolicited Canceled report for each order as its venue confirms it, when it next
logs on.

At shutdown the gateway logs its clients out, which starts no countdown, so it cancels the working
orders of every client session with cancel on disconnect on itself, whatever the trigger, logged
as one warning with the count per session. Other clients' orders stay working at their venues.

## API

With `[api]` in `jetbridge.toml` (see [Configuration](#jetbridgetoml)), the gateway serves an HTTP
JSON API for operators: the sessions, the book's orders, risk, routes and the message log to read,
and the kill switch, session controls, risk limits and reload to act with. Every answer is JSON,
an error as `{"error": "..."}` with its status.

### Users and tokens

Users are in the `users` file. The binary edits it, rewriting it whole (comments aren't kept), and
a reload (SIGHUP or `POST /api/reload`) applies the change. Run these as the gateway's user: the
rewritten file belongs to whoever runs them.

```sh
jetbridge user add alice --role operator --clients FUNDA --users users.toml   # asks for a password
jetbridge user passwd alice --users users.toml       # ends alice's sessions once the gateway reloads
jetbridge token add alice --users users.toml         # prints the token, once
jetbridge token revoke alice q3Xr --users users.toml # by the token, its id, or the id's start
jetbridge user remove alice --users users.toml
```

- **Roles**: `viewer` reads; `operator` also pauses, resumes and resends sessions, cancels orders
  and asks venues where they stand, resolves orders a venue doesn't know, and blocks, unblocks
  and cancels clients; `admin` also sets and resets sequence numbers, edits the risk limits,
  reloads and, without `--clients`, reads the audit log.
- **`--clients`** limits what a user sees as well as what it does: those CompIDs' client
  sessions, orders and scopes, and, for an admin, their limit tables. Such a user can't act on a
  venue's session, on `all`, or on the default limits or a reload, which reach every client.
  Without it a user sees and acts on every client; the API shows its `clients` as `null`, and
  with it, sorted and without repeats. A scoped user's reads:
  - `GET /api/orders`, past ones too, holds its clients' orders only. A `client` naming another
    client, or `all`, is 403 ("alice may see only clients FUNDA, not comp:FUNDB"), and so is
    another client's order by its OrderID, with its `/messages`. An order of its own shows its
    messages on both sides, the venue's too.
  - `GET /api/messages` searches its clients' sessions only, past ones too; a `session` naming
    another client's, or a venue's, is 403. Messages logged before a Logon, with no session, are
    left out.
  - `GET /api/sessions` lists its clients' sessions and every venue's. `GET /api/risk` has its
    clients' sessions, CompIDs and blocks (blocks of `all` too), with the defaults and every
    symbol's reference price, its last fill whichever client's it was; `GET /api/risk/limits` its
    clients' tables and the defaults.
  - `GET /api/routes` is every route; `GET /api/routes/test` refuses (403) a `client` not its own.
  - `GET /api/audit` is refused (403): it needs an admin without `--clients`.
  - `GET /api/live` sends what those reads show, and the alarms about its clients' orders; see
    [Live events](#live-events).
- **Passwords** are at least 12 bytes, kept as argon2 hashes. **Tokens** (`jb_...`) are kept as
  their SHA-256 only, so a lost token is revoked and replaced, not recovered.
- **Logging in.** A script sends `Authorization: Bearer <token>` on each request. A browser
  `POST`s `/api/login {"user", "password"}`, which sets a session cookie and answers
  `{user, role, clients, csrf}`; each request but a GET then carries `X-CSRF-Token: <csrf>` too
  (`GET /api/me` gives it again). `POST /api/logout` ends the session. A session ends after 8
  hours unused or 24 hours in all, and when its user is removed or changes password. A change to
  the users file applies, once reloaded, from the next request.
- **Failed logins**: five a minute for a user name, or for an address (an IPv6 address counts as
  its /64), then that name's or address's logins are refused (429) until a minute has passed
  since its first failure. Tokens aren't limited.

### Endpoints

| Endpoint | Role | Does |
| --- | --- | --- |
| `GET /api/health` | none | `{"version"}` |
| `POST /api/login`, `POST /api/logout`, `GET /api/me` | none, any, any | see above |
| `GET /api/sessions` | viewer | client sessions connected or paused, then every venue's: status, sequence numbers, activity, `paused` |
| `GET /api/orders` | viewer | the book's orders, or with `from` past ones: `?client=&venue=&symbol=&status=&working=true&from=&to=&after=&desc=true&limit=` |
| `GET /api/orders/{order_id}` | viewer | one order by its OrderID(37), past ones too, with every request sent for it and the venue's OrderID |
| `GET /api/orders/{order_id}/messages` | viewer | `?limit=&after=`: the order's messages, both sides'; see [Searching the message log](#searching-the-message-log) |
| `GET /api/messages` | viewer | `?session=&direction=&msg_type=&tag=&since=&until=&limit=&after=`: the message log, searched |
| `GET /api/risk` | viewer | the limits in force, each session's open and filled notional, reference prices, blocks |
| `GET /api/risk/limits` | viewer | the limits as `jetbridge.toml` writes them; see [Editing the risk limits](#editing-the-risk-limits) |
| `GET /api/routes` | viewer | the routes, in the order they're tried |
| `GET /api/routes/test` | viewer | `?client=&ex_destination=&symbol=`: the route such an order would take |
| `GET /api/live` | viewer | a websocket; see [Live events](#live-events) |
| `POST /api/sessions/{id}/pause` | operator | logs the session out and keeps it out until resumed |
| `POST /api/sessions/{id}/resume` | operator | lets it log on again |
| `POST /api/sessions/{id}/resend` | operator | `{from}`: asks the counterparty to send again from that MsgSeqNum; answers the sequence numbers |
| `POST /api/orders/{order_id}/cancel` | operator | the gateway's own cancel of one order; retries a stuck one |
| `POST /api/orders/{order_id}/status` | operator | asks the order's venue where it stands; answers 202 |
| `POST /api/orders/{order_id}/resolve` | operator | cancels, at the gateway, an order its venue doesn't know; answers 204 |
| `POST /api/risk/block`, `/unblock` | operator | `{scope}`: see [Kill switch](#kill-switch); answers `{"changed"}` |
| `POST /api/risk/cancel-all` | operator | `{scope}`: blocks the scope and cancels its orders |
| `POST /api/sessions/{id}/sequence` | admin | `{next_incoming, next_outgoing}`, either or both: sets the session's sequence numbers |
| `POST /api/sessions/{id}/reset` | admin | sets both sequence numbers to 1 and clears the session's resend store |
| `PATCH /api/risk/limits/defaults`, `/client/{comp_id}` | admin | `{set}`: edits the limits in `jetbridge.toml`, then reloads |
| `DELETE /api/risk/limits/client/{comp_id}` | admin | removes a CompID's limits from `jetbridge.toml`, then reloads |
| `POST /api/reload` | admin | rereads the files, as SIGHUP does, and says what changed |
| `GET /api/audit` | admin, without `--clients` | `?since=&limit=`: the audit log's last entries |

- **Every change needs a reason**: each POST, PATCH and DELETE but login and logout takes a JSON
  body with a non-empty `reason` (up to 1,000 bytes), or is refused (400).
- **Scopes** are `all`, `comp:COMPID` or `session:SESSION`, a session as `GET /api/sessions` shows
  it (`session:FIX.4.4:JETBRIDGE->FUNDA`). A session scope names one the gateway has seen.
- **A session in a path** is its `id` percent-encoded as `encodeURIComponent` does, `>` as `%3E` and
  a `/` (before a SubID or LocationID) as `%2F`: `/api/sessions/FIX.4.4:JETBRIDGE-%3EFUNDA/pause`.
  A client session can be acted on while connected or paused; otherwise it's 404.
- **Orders** come by OrderID in a path, and are paged by their numeric `id`: `GET /api/orders`
  answers `{orders, next}`, 100 by default and up to 1,000 with `limit`, and `next` is the `after`
  for the following page (`null` after a short page; a full page may be followed by an empty one).
  `client` is a scope (a session the gateway hasn't seen gives an empty page); `status` is `new`,
  `partially_filled`, `filled`, `canceled` and so on. Without `from`, the orders are the book's:
  working ones and each client's done orders of its current trading day. They come lowest `id`
  first; with `desc=true`, highest first, newest, and `after` reads on below it.
- **Past orders**, with a [database](#database): `GET /api/orders?from=&to=` reads the orders
  created from `from` up to `to`, both RFC 3339 times, `to` now by default. A range covers 31
  days at most, and may reach into the future. They're the database's orders, past days' and
  today's, oldest first (newest with `desc=true`), filtered as above; `client` may name a session that isn't connected now.
  Each is as last written, and an order pruned is gone. Here `next` is a string, not a number:
  pass it back as `after` as it is, with the same query. Write a `+` in a time's offset as `%2B`
  (`from=2026-10-01T09:00:00%2B02:00`), or it reads as a space. Refused (400): `to` without
  `from`, `from` not before `to`, a range over 31 days, and any range without a `[database]`.
- **An order the book no longer holds**, `GET /api/orders/{order_id}` and its `/messages` find it
  in the database. It shows `unknown_at_venue: false`, since that mark is kept in memory only, and
  its messages are found while the message log keeps them.
- **A routing dry run**, `GET /api/routes/test`, answers `{rule, route, target}`: `rule` is the
  index in `GET /api/routes` of the first route the order would match, `route` that route, and
  `target` `{"venue": NAME}` or `{"reject": TEXT}`; all three are `null` when none matches (the
  order would be rejected "no route for this order"). `client` is a CompID, as routes match it,
  not a scope; `client` and `symbol` are required. It's routing only: blocks, the ClOrdID limit
  and the risk limits aren't checked.
- **Asking a venue** where an order stands, `POST /api/orders/{order_id}/status`, sends it an
  OrderStatusRequest under the gateway's ClOrdID the venue last acknowledged, and answers 202: the
  venue's answer comes as the order's reports. One showing a fill the gateway missed reaches the
  client as a fill, one that changes nothing is dropped, and one saying the venue doesn't know the
  order raises an `unknown_at_venue` alarm and leaves the order as it was, marked: its view has
  `unknown_at_venue: true` until the venue reports on it after all or it's done. The mark is kept
  in memory only, so after a restart ask the venue again. Audited as `request_status`.
- **Resolving an order a venue doesn't know**, `POST /api/orders/{order_id}/resolve`, is the
  answer to that alarm once it's settled with the venue: the gateway cancels the order itself,
  keeping its fills and freeing its exposure, and the client gets an unsolicited Canceled with
  Text(58) "canceled by the gateway: the venue doesn't know the order". It answers 204 once that
  has committed; 404 for an order the book doesn't hold, and 409 for one that's done or that its
  venue hasn't said it doesn't know. Audited as `resolve`.
- **Sequence numbers.** `sequence` takes `next_incoming`, `next_outgoing` or both (numbers from
  1). The outgoing one only moves forward; a logged-on counterparty is told with a gap fill.
  `reset` works only while the session is disconnected: pause it first. For a reset agreed with
  the counterparty, it logs on with ResetSeqNumFlag(141) instead. Both answer `{next_incoming,
  next_outgoing}`, and 500 if the session's store fails. See
  [CAVEATS.md](CAVEATS.md#operator-api) before moving a live session's numbers.
- **What a refusal says**: 401 without valid credentials, 403 for the role or the CSRF token or a
  target out of a scoped user's CompIDs, 404 for a target that doesn't exist (whatever the user's
  scope), 409 for an order already done, a resolve of an order its venue hasn't said it doesn't
  know, a status request to a venue that isn't logged on, a
  resend the session can't make now (not logged on, a resend under way, or `from` not below the
  next expected), an outgoing sequence number below the next, or a reset while connected.
- **Reload** answers 200 with what changed in the sessions file, the routes, limits and `day_end`
  taken, and any warnings; or 422 if either file was kept, saying why and what the other one
  changed, since that one did apply. Its warnings include a changed `[database] url` or
  `[message_log]`, which waits for a restart. Reloads run one at a time, and one sent during
  shutdown answers 503.

### Editing the risk limits

`GET /api/risk/limits` answers the limits as `jetbridge.toml` writes them, `{"defaults": {...},
"clients": {"FUNDA": {...}}}`, each table with only the keys it sets, amounts as decimal strings
(`"1000"`, `"5.5"`) however the file writes them; `GET /api/risk` has them as in force, merged.
An admin changes them with a `PATCH` to `/api/risk/limits/defaults` or
`/api/risk/limits/client/{comp_id}`:

```json
{"reason": "more room for the open", "set": {"credit": "20000000.50", "symbols": null}}
```

- **`set`** names each key to change, the keys as [Risk limits](#risk-limits) lists them: an
  amount as a JSON number or, to keep every digit, a decimal as a string; `symbols` an array of
  strings; `self_trade` `true` or `false`. `null` removes the key: removed from a client's table,
  the client has the default's again. A client's table is created if it has none. An unknown key
  or a value of the wrong type is refused (400).
- **`DELETE /api/risk/limits/client/{comp_id}`** removes the client's table, so its sessions get
  the defaults; 404 if it has none.
- **`--clients`** applies: an admin with it edits only those CompIDs' tables (403 for another's).
  The defaults, like `POST /api/reload`, reach every client, so they need an admin without it.
- **The file is rewritten.** The gateway reads `jetbridge.toml` afresh, changes only that table,
  keeping the comments and layout elsewhere, checks the result loads, and replaces the file
  whole (the file a symlink points to, keeping its permissions). Then it reloads, as SIGHUP does,
  so the reload takes whatever else changed in the files since the last, the sessions file
  included. An edit the file wouldn't load with, or any edit while the file doesn't load as it
  is, is refused (422) and the file left as it was.
- **The answer** is `{table, reload}`: the table as the file now has it (`null` once deleted),
  and what the reload did, as `POST /api/reload` answers it; a reload that kept a file answers 422,
  though the edit is written (audited as failed, saying so). An admin with `--clients` gets
  `reload` as `null`, and a 422 for the rest of the file without why, since both can name other
  clients: the audit log has it, and an admin without `--clients` can see it with
  `POST /api/reload`. A bad key or value in the table it edits is said, to any admin. Edits and reloads run one at a time,
  and one sent during shutdown answers 503. Audited as `set_limits`, with the keys set and their
  values and the keys removed, or `delete_limits`, on `risk.defaults` or `risk.client.COMPID`.

### Searching the message log

With a [message log](#message-log), `GET /api/messages` finds the messages matching every condition
given, oldest first; without one, it answers 404 "no message log".

- `since`, an RFC 3339 time, is required. `until` is a day after it by default, and a search covers
  a day at most.
- `session`, a session's `id` as `GET /api/sessions` shows it, repeats: any of them.
- `direction` is `in` (received) or `out` (sent).
- `msg_type` repeats or takes a list (`msg_type=D,F`): any of them.
- `tag=11=ABC` repeats, and every one must hold. A search takes 100 sessions, MsgTypes and tags
  at most, together.
- `limit` is 100 by default, at most 1,000.

It answers `{messages, next, follow}`, each message
`{"time", "direction", "session", "msg_type", "fields": [[11, "ABC"], ...], "raw"}`, `raw` as on
the wire with `|` for SOH. `session` is `null` for a client's messages before its Logon, and
`msg_type` `null`, with no fields, for one that doesn't decode.

- **Paging.** `next` is a cursor: pass it as `after`, with the same query, for the next page. A
  search reads up to 256 MiB of the log per page, so a page can come back short, or empty, with a
  `next`. `next` is `null` once the range is read to the end of what's logged.
- **Following.** While the range's end is still to come (until two seconds past it), the last page
  also has `follow`, the end of what's logged so far: passed as `after` later, it finds what's
  been logged since. A live order's search has one.
- **Refusals**: 400 for an unknown parameter, one given twice that can't repeat, an empty value,
  over 100 conditions, `until` not after `since`, a range over a day, or an `after` that isn't a
  cursor a search gave (or whose file has gone); 503 while two searches are under way.

`GET /api/orders/{order_id}/messages?limit=&after=` finds an order's messages on both sides, paged
the same way: its client session's with any of the order's ClOrdIDs in ClOrdID(11) or
OrigClOrdID(41), and its venue session's with any of the gateway's ClOrdIDs for it in either, or
the venue's OrderID in OrderID(37), from a minute before the order was booked, for a day.

### Live events

`GET /api/live` upgrades to a websocket. Anything but a browser authenticates the upgrade with a
bearer token; a browser with its session cookie, from a page on the API's own host (its `Origin`
must name the host it's sent to, over `https` when the cookie is `Secure`). A token is never taken
in the URL. The socket first sends a snapshot,

```json
{"type":"snapshot","sessions":[...],"orders":[...],"orders_next":null,"blocked":["comp:FUNDA"],"unavailable":false,"clients":["FUNDA"]}
```

with the sessions as `GET /api/sessions` has them and the working orders, up to 1,000: with a full
page, `orders_next` is the `after` to read the rest from `GET /api/orders?working=true`.
`clients` is the user's clients as of the snapshot (`null` for a user without `--clients`). Then
each change, in the order it happened:

- `{"type":"order", ...}`: an order's whole view, as `GET /api/orders` has it, to put in place of
  the one held, or add.
- `{"type":"blocked","scope":...}` and `{"type":"unblocked","scope":...}`.
- `{"type":"alarm","time","kind","client","text"}`, `kind` one of `gateway_unavailable`,
  `gateway_available`, `stuck_cancel`, `orphan`, `overfill`, `unknown_order`, `unknown_at_venue`
  (a venue answered a status request saying it doesn't know a live order), `fill_on_done_order`
  (a venue reported a fill on an order the book has as done, after a resolve or crossing the
  venue's own cancel: not applied) and `venue_store_reset`. Each is in the log too. `client` is
  the session of the order the alarm is about (`stuck_cancel`, `overfill`, `unknown_at_venue`,
  `fill_on_done_order`), or `null`.
- `{"type":"session","side","id","event"}` for the session keyed by `side` (`client` or `venue`)
  and `id`: `connected`, `logged_on`, `logging_out`, `resend_started`, `resend_finished`,
  `disconnected`, `paused`, `resumed`, or `refused` with a `reason`.

A user with `--clients` gets the snapshot and events of its clients' sessions and orders, every
venue's session, its clients' blocks and those of `all`, and of the alarms, `gateway_unavailable`,
`gateway_available`, `venue_store_reset` and those whose `client` is its own: `orphan` and
`unknown_order` go only to users without `--clients`. When a reload changes a user's clients, an
open socket gets a snapshot under the new ones within 10 seconds, its `clients` the new ones; until
then it keeps sending what the old ones showed. No snapshot repeats the alarms: those raised for a
client the user no longer has are for the console to drop.

The first few events may repeat what the snapshot shows. A socket that falls behind skips what it
missed and gets a new snapshot; one that falls behind again within a second is closed with code
1013 (too slow: reconnect). The gateway pings every 10 seconds and closes a socket it has heard
nothing from for 30 seconds (1001); a socket also closes when the gateway stops (1001), and within
10 seconds of its session ending or its token being revoked (1008). An open socket doesn't count
as using its session, which still ends after 8 hours unused. 64 sockets may be open at once (503
past that), 8 of them a user's (429). What the client sends is ignored.

### Where it listens

`listen` on loopback serves plain HTTP. Anywhere else the API needs `tls = { cert, key }`, or
`behind_proxy = true` for a proxy in front that terminates TLS: the passwords and tokens mustn't
cross the network in the clear. It serves HTTP/1.1, websockets included. Behind a proxy:

- the client's address, for the login limit and the audit log, is the last `X-Forwarded-For` hop,
  and a page's `Origin` is checked against `X-Forwarded-Host` if sent, else `Host`. The proxy must
  set both, overwriting what the client sent;
- the session cookie is `__Host-jetbridge_session`, `Secure` with `Path=/`, as under TLS (on
  plain loopback it's `jetbridge_session` with `Path=/api`). The browser sends it to every path on
  that host name, so give the API a host name of its own.

### Audit log

Every change through the API, and every login and logout, is appended to the audit log as one
JSON line, synced to disk before it runs: a line that can't be written answers 503 and nothing is
done. The lines go in `audit_dir`, a file a day, `audit-YYYY-MM-DD.jsonl` by the entry's UTC date.

```json
{"time":"2026-10-11T09:30:00.123456Z","user":"alice","role":"operator","ip":"192.0.2.1","action":"block","target":"comp:FUNDA","params":null,"reason":"runaway algo","result":"allowed"}
```

`result` is `allowed` or `refused: <why>`. An allowed change that didn't simply get done gets a
second line: `failed: <why>`, or what happened instead, such as `unchanged: already blocked`.
Refusals for a user's role, scope or a missing reason are recorded too, and so are failed and
limited logins, wrong tokens, and wrong CSRF tokens or `Origin`s, at most 120 a minute of these
(the rest are in the tracing log only). A failed login whose name isn't a user's is recorded as
`<unknown>`. Reads aren't recorded, nor requests with no credentials or an ended session.

The directory is created readable by its owner only (0700) if missing, and so is each file. With
`audit_retention` (a whole number and `s`, `m`, `h` or `d`, as `[message_log]`'s `retention`), the
first line of each day deletes the files of days that ended longer ago than that; other files in
the directory aren't touched, and a delete that fails is logged as a warning. Left out, the files
are kept for ever. `audit_dir` and `audit_retention` are read at start only. `GET /api/audit` reads
it back, oldest first: the last `limit` entries (100 by default, at most 1,000) from `since` (RFC
3339), reading only the days it needs, newest first.

## Console

A web console for operators, served by the gateway at the API's address, at `/`: with the
example's `[api]`, <http://127.0.0.1:8080/>. Log in as one of the API's [users](#users-and-tokens);
it shows and does what that user's role and clients allow, as the API does. It's built into the
binary, so build it first, with Node 24 or later (`web/package.json`'s `engines`), then the gateway
with `--features console`:

```sh
(cd web && npm ci && npm run build)
cargo build --release --features console
```

Without the feature, the gateway serves the API only. The console's files are served with a
Content-Security-Policy allowing only the gateway's own scripts, styles and connections.

Its screens are kept live by `GET /api/live`, with a banner while that socket is down. Every user
sees Orders, Sessions, Risk, Messages and Routes in the nav; an admin without `--clients` also sees
Audit, and a Reload button in the header. A user with `--clients` sees and acts on only its clients,
as the API narrows it. A reload that changes a user's clients shows at its live socket's next
snapshot, when the console also reads the user's role again. Viewers read; operators and admins act,
each action with a reason:

- **Orders**, the blotter: the working orders, newest first, filtered by client, venue, symbol and
  status; with "working only" off, today's done orders too; with dates, past days' orders from the
  [database](#database), as they were read. Each links to its page.
- **An order's page**: its fields, kept live; its requests to the venue; and its messages on both
  sides from the [message log](#message-log), a page at a time. An operator or admin can cancel it,
  ask its venue for its status, and resolve it once the venue has said it doesn't know it.
- **Sessions**: the client and venue sessions, their state and next sequence numbers, and whether
  paused. An operator or admin can pause, resume, or ask for a resend from a sequence number. An
  admin can also set the next sequence numbers, either or both, warned that lowering next in while
  logged on can have messages accepted again, so pause the session first; or reset both to 1,
  which works only while the session is disconnected and clears the messages kept for resends.
  See [CAVEATS.md](CAVEATS.md#operator-api).
- **Risk**: each session's open and filled notional beside its limits, the reference prices and the
  blocks, read again every 5 seconds while the screen is open and the page shown. An operator or
  admin can block a scope, lift a block, or pull the [kill switch](#kill-switch) for a scope, which
  blocks it and cancels its working orders, confirmed by typing the scope. Below, the limits as
  `jetbridge.toml` writes them, each client's table with only the keys it sets. An admin edits a
  key, removes it, or adds or deletes a client's table, the defaults only without `--clients`; each
  edit [rewrites the file and reloads](#editing-the-risk-limits), and shows the table as written and
  what the reload took.
- **Messages**: the [message log](#searching-the-message-log) searched by session, direction,
  MsgTypes (`D,F`) and tag conditions, every one of which must hold, over a range from since to
  until (UTC; a day after since if left empty) of 24 hours at most, with 100 sessions, MsgTypes
  and tags at most together, and 1 to 1,000 a page (100 by default). "More" reads the next page,
  and while the range's end is still to come, "Check for newer" reads what's been logged since.
  Messages show as text, as on the wire.
- **Routes**: the routing rules in the order they're tried, and a dry run: a client's CompID,
  ExDestination and symbol, and the rule such an order would match, highlighted, with the venue it
  would go to or the reject it would get. Routing only: blocks and risk aren't checked.
- **Reload**, from the header: rereads the files, as SIGHUP does. What it took, or why it kept a
  file, shows under the header on every screen until dismissed.
- **Audit**: the [audit log](#audit-log), newest first, from a since (UTC; the start if empty), the
  last 100 entries or up to 1,000 with a limit. It can't page back past those: a full read says
  older ones are left out.

Alarms show above every screen, newest first, the latest 100, each with its time, kind, client and
text, until dismissed; the header counts those not dismissed. While the gateway is
[unavailable](#database), a banner says so.

What to expect of it is in [CAVEATS.md](CAVEATS.md#console).

To work on it, run the gateway with an `[api]`, then `npm run dev` in `web/`: Vite serves the
console, reloading on changes, and passes `/api`, websockets included, to `JETBRIDGE_API`
(`http://127.0.0.1:8080` by default). `npm test -- --run` runs its tests and `npm run lint` checks
its types. `cargo test --all-features` includes the console, so needs `web/dist` built first.

## Signals

- **SIGHUP** (Unix) reloads. The sessions file first, as turbojet-config reloads it: changed
  settings apply from each session's next logon, added venues start and removed ones are logged out
  and stopped. Then the routes, risk limits and `day_end`, the routes checked against the venues
  running after that. A file that doesn't load is logged as a warning and the one in use is kept: a
  bad sessions file keeps the sessions as they were, and a bad `jetbridge.toml` keeps the routes,
  limits and `day_end` (an `[api]` that doesn't load, or breaks the bind rule, makes the whole file
  bad). Then the API's certificate, with `[api] tls`. New routes and limits apply to new orders;
  orders already working stay with their venue. A new `day_end` or schedule applies from each
  session's next trading day.
- **Ctrl-C or SIGTERM** stops the API (waiting up to 5 seconds for its requests under way), logs
  every client out ("gateway shutting down"), which fires any cancel-on-disconnect countdowns under
  way, and cancels the working orders of clients with cancel on disconnect (see above); waits up to
  5 seconds for the venues to answer the gateway's cancels; then logs every venue out, waits for the
  last changes to commit to the database, if there is one, and exits. A second signal stops
  waiting, but the exit still waits for work under way on blocking threads, such as a message log
  search (each reads up to 256 MiB) or a `.log.gz` decompressed up to a cursor, and for the message
  log to close its files, gzipping the last with `compress = true`: bounded, but it can take
  seconds.
  On the way out it logs how many orders are still open at each venue and how many reports weren't
  delivered to each client. With a database, the next start restores both. Without one, both are
  lost: the open orders stay working at their venues with nothing to report their fills to.

## Logging

Logs go to standard output through `tracing`, at `info` by default. `RUST_LOG` sets the filter, as
`tracing-subscriber`'s `EnvFilter` reads it:

```sh
RUST_LOG=info,jetbridge=debug cargo run --release -- --config examples/jetbridge.toml
```

Reports the gateway can't use (a trade bust, a report taking an order past its quantity, a
venue's code it doesn't model) are logged as errors or warnings naming the venue and its ClOrdID.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)).
