# Jetbridge

A FIX connectivity gateway built on [Turbojet](https://github.com/jrogers/turbojet): clients log
on to it, it logs on to venues, and it routes the clients' orders to the venues and the venues'
reports back, keeping each order's state as it goes.

> [!WARNING]
> **Early work.** Orders are kept in memory only: a restart forgets every order and every report
> not yet delivered. It hasn't been run against a real venue or client. Read
> [CAVEATS.md](CAVEATS.md) before relying on it, and don't use it to trade real money without
> testing it thoroughly yourself. It is provided as is, without warranty of any kind.

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
- **Reload on SIGHUP** of the routes and the sessions file, keeping connected sessions the file
  still has.

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
# Jetbridge: the sessions file, and how orders are routed. First match wins; an order no route
# matches is rejected.
sessions = "sessions.toml"

# FUNDA's orders for XNAS go to NASDAQ.
[[route]]
client = "FUNDA"
ex_destination = "XNAS"
venue = "NASDAQ"

[[route]]
reject = "no route for this order"
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
  one CompID with different SubIDs or LocationIDs each have their own ClOrdIDs and reports.
- Each `[initiator.NAME]` is a venue, and `NAME` is what routes call it. A venue speaks its own
  `begin_string` (the acceptor's by default), `FIX.4.2` or `FIX.4.4`, and logs on as the
  acceptor's `sender_comp_id` unless it sets its own. A venue with another version stops the
  gateway at start; a reload that adds one skips it, with an error logged.
- Sessions are kept in the `memory` store unless the file says otherwise, so sequence numbers
  start again at each restart. A `[store.NAME]` with `kind = "disk"` keeps them.

## Kill switch

An operator can stop a client trading. For now these are library calls on the running gateway
(`Service::gateway()`); operator access over HTTP is planned for a later stage. Each takes a
`Scope`: one client session, a CompID (all its sessions, including ones that log on later), or
everyone.

- `cancel_all(scope)` blocks the scope and cancels its clients' live orders at their venues, in
  one step. Each client gets an unsolicited Canceled report for each order as its venue confirms
  it. A cancel the venue refuses is sent again, up to three times; `cancelling()` lists the
  cancels still going and those given up. A cancel for a venue that is down goes when it logs
  back on; one its session couldn't queue is tried again within a second.
- `block(scope)` only blocks: a blocked client's new orders are rejected ("blocked by an
  operator"). Its cancels pass, and so do replaces that don't raise the quantity.
- `unblock(scope)` lifts the block of that scope exactly: unblocking a session leaves a block of
  its CompID in force. `blocked()` lists the blocked scopes.

Each call is logged as a warning naming its scope.

## Signals

- **SIGHUP** (Unix) reloads. The sessions file first, as turbojet-config reloads it: changed
  settings apply from each session's next logon, added venues start and removed ones are logged
  out and stopped. Then the routes, checked against the venues running after that. A file that
  doesn't load is logged as a warning and the one in use is kept: a bad sessions file keeps the
  sessions as they were, and a bad routes file keeps the routes. New routes apply to new orders;
  orders already working stay with their venue.
- **Ctrl-C or SIGTERM** logs every client and venue out ("gateway shutting down") and exits once
  they've closed; a second signal exits at once. On the way out it logs how many orders are still
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
