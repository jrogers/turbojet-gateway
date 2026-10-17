# Changelog

Notable changes to Turbojet Gateway.

## Unreleased

- A FIX gateway, `turbojet-gateway --config turbojet-gateway.toml`: clients log on to a Turbojet
  acceptor, the gateway logs on to each venue with a Turbojet initiator, both set up by a
  `turbojet-config` sessions file, and orders are routed between them. FIX 4.2, 4.4 and 5.0 SP2
  (over FIXT.1.1) on either side, across versions.
- Clients send NewOrderSingle, OrderCancelRequest, OrderCancelReplaceRequest and
  OrderStatusRequest, for Market and Limit orders, and get ExecutionReports and
  OrderCancelRejects. Venues get orders, cancels and replaces under the gateway's own ClOrdIDs,
  and their reports reach the client under its ClOrdIDs with the gateway's OrderIDs and ExecIDs.
  Each of the gateway's IDs starts with a prefix from its start time, so a restart doesn't reuse
  them. A venue's OrdRejReason or CxlRejReason passes through as sent, listed in the client's
  FIX version or not.
- An order book: each order's state, ClOrdID dedupe per client (a possible resend is
  answered with the order's state), one replace pending at a time, stale and resent venue reports
  dropped rather than rolling an order back, overfills refused, and the venue's reject reasons
  passed through in the client's version.
- Routing rules in `turbojet-gateway.toml`, `[[route]]`s matching on client, ExDestination and
  Symbol, first match wins, to a venue or rejected with a text.
- A client is a session: sessions of one CompID with different SubIDs or LocationIDs each keep
  their own ClOrdIDs, reports and risk counts, up to 16 per CompID. Routes and limits match the
  CompID.
- Risk limits in `turbojet-gateway.toml`, `[risk.defaults]` and `[risk.client.COMPID]`: maximum
  order quantity and notional, allowed symbols, a price band around the symbol's last fill, credit
  (open plus filled today) and self-trade prevention, checked per session on each order and
  replace after routing. A replace that adds no risk passes. Refused orders get OrdRejReason 3
  for quantity, notional and credit, and a Text naming the limit.
- A trading day per client session, ending at `day_end` in `turbojet-gateway.toml` (a time and time
  zone; midnight UTC by default) or when its schedule's period ends, whichever comes first. When
  it ends, the session's done orders and their ClOrdIDs are dropped, its ClOrdID count and filled
  notional start again, and its live orders carry over. A possible resend of an order first sent
  before the session's day began, and not known, is rejected as a duplicate.
- Reports for a client that's disconnected or slow are held in a bounded queue per client and
  delivered in order once it can take them.
- An order whose venue isn't connected, or whose session drops it unsent, is rejected as venue
  unavailable.
- A client's order, cancel or replace stays in flight on its session (Turbojet's
  `Context::hold`) until the venue's session has stored what it was forwarded as, so a client
  connection lost before then resends it rather than losing it.
- An optional database, `[database] url = "sqlite://...?mode=rwc"` in `turbojet-gateway.toml`
  (SQLite; a missing file is an error unless the URL asks for it to be created; read at start only).
  The book's orders, ClOrdIDs and requests sent, operators' blocks, risk counts and the reports not
  yet delivered are committed in the background, without delaying orders or reports, and
  restored at the next start. Every
  message that changed the book is held in flight until its change has committed. Without a
  `[database]` everything stays in memory, as before.
- PostgreSQL for `[database] url`, with `--features postgres`, and TLS to it with `tls`.
- Recovery at start: before the venues log on, each venue's session store is read and compared with
  the database, from a checkpoint taken every minute rather than whole. An order the venue's session
  stored but the database never recorded is cancelled at the venue's logon, logged as an error; an
  unrecorded cancel cancels its order, and an unrecorded replace stays pending on its order,
  Replaced to the client if it sends its replace again with those terms, marked as a resend or not,
  Restated if not. Requests the database has that the venue's session never stored are sent at the
  venue's logon, marked PossResend, unless the session resets its sequence numbers at that logon,
  when they fail. Undelivered reports go to their clients again, marked PossResend.
- New orders are rejected as "gateway unavailable" while the oldest change not yet committed has
  waited over a second; cancels, replaces and venue reports keep flowing.
- With a database, client and venue sessions must be kept in a durable store: a start with a
  sessions file that keeps one in memory stops, and a reload that does logs an error. A start
  that finds a venue's store with no record of its session while the database has live orders,
  or orders of a client's current day, for that venue stops too (its files deleted, say).
- Each start waits for the next second before taking its ID prefix, so two starts never share
  one unless the clock goes back.
- Reports made from a message that may have been handled before a restart are marked
  PossResend.
- A venue report that changes nothing (a repeated ack) is dropped rather than forwarded under a
  new ExecID (restatements, replaces and DoneForDay still go).
- A kill switch, as library calls on the gateway: block a client session, a CompID or everyone
  (new orders rejected; cancels and replaces that don't raise the quantity pass), cancel their
  orders at the venues, and unblock. The gateway's own cancels are retried when a venue refuses
  them (three tries in all), sent when a down venue logs back on, and listed while they're going;
  clients get an unsolicited Canceled report per order.
- Cancel on disconnect: a client session with `cancel_on_disconnect` in the sessions file has
  its orders cancelled at the venues when it doesn't log back on within its grace, and at
  shutdown, which waits up to 5 seconds for the venues to answer the gateway's cancels.
- SIGHUP reloads the sessions file, the routes, the risk limits and `day_end`, keeping either file if it
  doesn't load; Ctrl-C or SIGTERM logs every session out and says what the stop leaves open.
  Optional TLS (feature `tls`).
- An operator API, with `[api]` in `turbojet-gateway.toml`: HTTP and JSON on its own port, on
  loopback unless it has TLS (feature `tls`) or `behind_proxy = true`. Viewers read the sessions,
  the book's orders (working ones and each client's current day), risk usage and the routes;
  operators pause, resume and resend sessions, cancel an order and use the kill switch; admins
  reload and read the audit log. `GET /api/live` is a websocket sending a snapshot, then the
  orders, blocks, alarms and session events as they happen.
- Users in a `users` file, edited with `turbojet-gateway user add|passwd|remove` and
  `turbojet-gateway token add|revoke`: argon2 passwords for a browser session with a CSRF token, or
  bearer tokens kept hashed, in three roles, optionally limited to some CompIDs. Failed logins are
  limited per name and per address.
- An audit log: every change through the API, and every login and logout, as a JSON line synced
  to disk before it runs, with who, from where, why and what came of it; refusals too.
- More of the API: a routing dry run (`GET /api/routes/test`); setting and resetting a session's
  sequence numbers; asking a venue where an order stands (`POST /api/orders/{order_id}/status`),
  with an `unknown_at_venue` alarm when it doesn't know a live order; the risk limits as
  configured (`GET /api/risk/limits`, amounts as decimal strings), and editing them, which
  rewrites `turbojet-gateway.toml`'s table in place and reloads. The order detail has the venue's
  OrderID.
- A venue's ExecutionReport saying it doesn't know an order (Status, Rejected, OrdRejReason 5) no
  longer rejects the order: it raises the `unknown_at_venue` alarm, and the order stays live.
- A message log, with `[message_log]` in `turbojet-gateway.toml`: every message both sides' sessions
  receive and send, in files by UTC day, passwords masked, with size, retention and gzip
  compression (feature `gzip`). `GET /api/messages` searches it by session, direction, MsgType,
  tags and time, and `GET /api/orders/{order_id}/messages` finds an order's messages on both
  sides.
- A reload that finds `[database]` or `[message_log]` changed says so in its warnings.
- `POST /api/orders/{order_id}/resolve`: an operator cancels, at the gateway, an order its venue
  has said it doesn't know, its fills kept and its exposure freed; the client gets Canceled. An
  order's view has `unknown_at_venue` while it can be resolved.
- A fill a venue reports on an order the book has as done is no longer dropped silently: it
  raises a `fill_on_done_order` alarm.
- The audit log is written in daily files, `audit-YYYY-MM-DD.jsonl` in `[api] audit_dir`, with an
  optional `audit_retention` deleting old days, and `GET /api/audit` reads only the days it
  needs. **Breaking:** `[api] audit` is gone, and a file that still sets it doesn't load.
- `[database] retention`, optional: every hour the gateway deletes the done orders of trading
  days that ended longer ago than that, with their requests sent and ClOrdIDs. Without it nothing
  is deleted.
- `GET /api/orders` reads past orders with `from` and `to` (RFC 3339, a range of 31 days at
  most), from the database, so it needs a `[database]`; its pages' `next` is then a string to
  pass back as `after`. An order's detail and messages are found after the book drops it.
- `GET /api/orders?desc=true` pages newest first, the book's orders and past ones alike.
- The live socket's snapshot carries `clients`, the user's clients it was taken under (`null` for
  all), so a console can tell when a reload rescoped it.
- A web console in `web/` (React and TypeScript, Node 24 or later to build), served at the API's
  address by a gateway built with `--features console`: the blotter live and past days', an
  order's page with cancel, status and resolve, the sessions with pause, resume and resend, and
  the alarms, with a banner while the gateway is unavailable. Also risk, with exposure, blocks,
  the kill switch (confirmed by typing its scope) and, for admins, limit editing; message search;
  setting and resetting sequence numbers, for admins; the routes with a dry run; and, for admins
  without `--clients`, reload and the audit log. The nav shows each user what its role and
  clients allow. A reason dialog returns the focus where it was when it closes.
- The console has a new look: dark first (light when the system asks), a sidebar with live
  counts, status pills, fill bars, credit used against each limit, and rows that flash as the
  feed changes them.
- Marking an order unknown at its venue, and clearing that mark, now sends an `order` event on the
  live socket: a console saw the change only at its next snapshot.
- The first start of this version on a database indexes its requests sent by time, once, for
  reading past orders: on a large database that start takes longer.
- A user limited to some CompIDs (`--clients`) now reads only those CompIDs' clients, as it
  already acted only on them: its orders, messages, sessions, risk, limits and live socket are
  narrowed to them, and a read naming another client is 403. Venue sessions, the routes and the
  reference prices stay visible to every user. `GET /api/audit` needs an admin without
  `--clients`, and a limit edit by an admin with them doesn't answer what its reload did. A
  user's clients are kept sorted and without repeats.
- Alarms carry `client`: the session of the order an alarm is about, or `null`. A scoped user's
  live socket gets the gateway-wide alarms and those about its clients' orders.
- A database connection lost without a reset fails the write in hand within 10 seconds, rather
  than holding the book's writer for minutes, or for good. Prune batches and reads of past orders
  time out the same way.
- An operator's action answers within 5 seconds during a database outage: 202, `committed:
  false`, the change in effect and committed when the database answers.
- **Security:** a client whose CompID, SubID or LocationID, or the SubID or LocationID it
  addresses the gateway by, contains '/', ':' or '>' is refused at logon. Its session's name
  could print the same as another session's, so under `unknown = "admit"` a client logging on as
  `FUNDA/X` could take over the restored orders and risk counts of FUNDA's desk X. After upgrading,
  an existing client with such a name is refused at logon, and its restored orders stay
  unclaimed: an operator can cancel them, or resolve those a venue doesn't know, or have the
  counterparty log on under a name without those characters.
- **Security:** a database URL's password given as a `password` query parameter (its name
  percent-encoded or not), or holding `/`, `?` or `@`, was logged unmasked.
