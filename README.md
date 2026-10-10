# Jetbridge

A FIX connectivity gateway built on [Turbojet](https://github.com/jrogers/turbojet): clients log
on to it, it logs on to venues, and it routes the clients' orders to the venues and the venues'
reports back, keeping each order's state as it goes.

> [!WARNING]
> **Early work.** Everything is kept in memory only: a restart forgets every order, every report
> not yet delivered, operators' blocks and what the risk limits have counted. It hasn't been run
> against a real venue or client. Read [CAVEATS.md](CAVEATS.md) before relying on it, and don't
> use it to trade real money without testing it thoroughly yourself. It is provided as is,
> without warranty of any kind.

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
- **Reports held per client.** A report for a client that's disconnected, or reading slowly, waits
  in that client's queue and goes out, in order, once it can take it.
- **Risk limits** per CompID, checked on each order and replace before it goes to a venue: order
  quantity, notional, allowed symbols, a price band around the last fill, credit and self-trade
  prevention.
- **A kill switch**, as library calls: block a client session, a CompID or everyone, and cancel
  their orders at the venues.
- **A trading day** per client session, from its schedule or a daily `day_end`: at its end, done
  orders and their ClOrdIDs go, and the session's ClOrdID count and filled notional start again.
- **Cancel on disconnect**, set per counterparty in the sessions file: a client's orders are
  cancelled when its session drops and it doesn't log back on in time, and at shutdown.
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

Jetbridge builds against Turbojet by path, so it expects a Turbojet checkout next to it
(`../turbojet`). It needs Rust 1.89 or later.

## Configuration

Two files: `jetbridge.toml`, given with `--config`, and the sessions file it names.

### jetbridge.toml

```toml
# Jetbridge: the sessions file, how orders are routed and the clients' risk limits. First match
# wins; an order no route matches is rejected.
sessions = "sessions.toml"
day_end = "17:00 America/New_York"  # a session without a schedule's end of trading day

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
- `day_end`: when a client session without a schedule ends its trading day: see
  [Trading day](#trading-day).
- `[risk.defaults]` and `[risk.client.COMPID]`: see [Risk limits](#risk-limits).

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
  start again at each restart. A `[store.NAME]` with `kind = "disk"` keeps them.

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

## Trading day

Each client session has a trading day. When it ends, the session starts the next one:

- its done orders (filled, cancelled, expired, rejected) are forgotten, and their ClOrdIDs can be
  used again;
- its live orders (GTC ones, or any still working) carry over, and their ClOrdIDs stay in use;
- its count of ClOrdIDs against the 1,000,000 a day starts again from its live orders' ClOrdIDs;
- what it has filled, for `credit`, starts again at zero.

Symbols' last fill prices are every client's, and carry over.

A session's day is its schedule's period, from the sessions file (`schedule = "daily
08:00-17:00 America/New_York"`, say): it ends when the period ends, and between periods the
session is in the next period's day. A session without a schedule ends its day at `day_end` in
`jetbridge.toml`: a time, `HH:MM` or `HH:MM:SS`, and a time zone, `UTC`, an offset such as
`+05:30` or an IANA name such as `America/New_York` (daylight saving followed). The zone is UTC if
left out, and `day_end` is midnight UTC if left out.

The day rolls over at a session's first message after its end, before that message is handled,
or within a second by itself if the session is quiet. A SIGHUP rereads `day_end` with the routes
and limits; a new `day_end`, or a changed schedule, applies from each session's next day: the day
under way ends when it was going to.

## Kill switch

An operator can stop a client trading, with library calls on the running gateway
(`Service::gateway()`). Each takes a `Scope`: one client session, a CompID (all its sessions,
including ones that log on later), or everyone.

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

## Signals

- **SIGHUP** (Unix) reloads. The sessions file first, as turbojet-config reloads it: changed
  settings apply from each session's next logon, added venues start and removed ones are logged
  out and stopped. Then the routes, risk limits and `day_end`, the routes checked against the
  venues running after that. A file that doesn't load is logged as a warning and the one in use is
  kept: a bad sessions file keeps the sessions as they were, and a bad `jetbridge.toml` keeps the
  routes, limits and `day_end`. New routes and limits apply to new orders; orders already working
  stay with their venue. A new `day_end` or schedule applies from each session's next trading day.
- **Ctrl-C or SIGTERM** logs every client out ("gateway shutting down"), which fires any
  cancel-on-disconnect countdowns under way, and cancels the working orders of clients with cancel
  on disconnect (see above); waits up to 5 seconds for the venues to answer the
  gateway's cancels; then logs every venue out and exits once they've closed. A second signal
  exits at once. On the way out it logs how many orders are still
  open at each venue and how many reports weren't delivered to each client. Both are lost: the open
  orders stay working at their venues with nothing to report their fills to.

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
