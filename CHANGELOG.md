# Changelog

Notable changes to Jetbridge.

## Unreleased

- A FIX gateway, `jetbridge --config jetbridge.toml`: clients log on to a Turbojet acceptor,
  the gateway logs on to each venue with a Turbojet initiator, both set up by a
  `turbojet-config` sessions file, and orders are routed between them. FIX 4.2 and 4.4 on either
  side, across versions.
- Clients send NewOrderSingle, OrderCancelRequest, OrderCancelReplaceRequest and
  OrderStatusRequest, for Market and Limit orders, and get ExecutionReports and
  OrderCancelRejects. Venues get orders, cancels and replaces under the gateway's own ClOrdIDs,
  and their reports reach the client under its ClOrdIDs with the gateway's OrderIDs and ExecIDs.
  Each of the gateway's IDs starts with a prefix from its start time, so a restart doesn't reuse
  them.
- An order book in memory: each order's state, ClOrdID dedupe per client (a possible resend is
  answered with the order's state), one replace pending at a time, stale and resent venue reports
  dropped rather than rolling an order back, overfills refused, and the venue's reject reasons
  passed through in the client's version.
- Routing rules in `jetbridge.toml`, `[[route]]`s matching on client, ExDestination and Symbol,
  first match wins, to a venue or rejected with a text.
- Reports for a client that's disconnected or slow are held in a bounded queue per client and
  delivered in order once it can take them.
- An order whose venue isn't connected, or whose session drops it unsent, is rejected as venue
  unavailable.
- A kill switch, as library calls on the gateway: block a client session, a CompID or everyone
  (new orders rejected; cancels and replaces that don't raise the quantity pass), cancel their
  orders at the venues, and unblock.
- Cancel on disconnect: a client session with `cancel_on_disconnect` in the sessions file has
  its orders cancelled at the venues when it doesn't log back on within its grace, and at
  shutdown.
- SIGHUP reloads the sessions file and the routes, keeping either if it doesn't load; Ctrl-C or
  SIGTERM logs every session out and says what the stop leaves open. Optional TLS (feature
  `tls`).
