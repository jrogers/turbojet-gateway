# Known caveats

Behaviour that is deliberate or documented, but worth knowing about before you rely on Jetbridge.
Turbojet's own are in its [CAVEATS.md](https://github.com/jrogers/turbojet/blob/main/docs/CAVEATS.md).
What's still open is in [ROADMAP.md](ROADMAP.md).

## The book

- Every ID the gateway issues (OrderIDs, ExecIDs and the ClOrdIDs sent to venues) starts with a
  prefix from the start time: six base-36 digits of the seconds since 2026, such as `0EI800` in
  `0EI8001-1`. Each start waits for the next second before taking its prefix, so a start takes up
  to a second longer, and two starts don't share one unless the clock goes back; with a database,
  a start that finds a venue holding a request under its own prefix stops, saying so. The prefix
  takes six of a venue's ClOrdID length limit.
- A venue session that's slow to store, or stuck, holds back the clients whose requests it hasn't
  stored: 256 messages past the oldest one waiting, a client's session stops reading from it until
  the venue's session catches up, its requests for other venues and its cancels included.
  Turbojet logs a warning when that happens. Its `turbojet_held_back` gauge needs Turbojet's
  `metrics` feature, which Jetbridge isn't built with. A database that's slow or down holds
  sessions back the same way (see below).
- A gateway cancel (the kill switch's) that a venue's session can't queue is tried again once a
  second while the session is connected, without limit.
- A venue's OrdRejReason or CxlRejReason reaches the client as the venue sent it, even a code
  the client's FIX version doesn't list; a client whose engine checks codes against its
  dictionary may refuse the message.
- At the end of a session's trading day its done orders, their ClOrdIDs and requests sent, and
  its filled notional are dropped from memory (see the README's trading day); with a database
  they stay in it as history. Live orders carry over, GTC ones for as long as they work, and so
  do last fill prices, one per symbol traded. The bound of 1,000,000 ClOrdIDs per client (plus
  10,000 for cancels and replaces once it's reached) counts per day, from the ClOrdIDs of the
  live orders carried over.

## Without a database

- The book (orders, the ClOrdIDs each client has used, what was sent to each venue), the reports
  held for clients, operators' blocks and what the risk limits count live in memory. A stop
  loses them; shutting down logs how many open orders and undelivered reports it leaves. Open
  orders stay working at their venues.
- After a restart, numbering starts again at 1 under the new prefix. A late report from a venue
  on an order from before the restart names an unknown order and is dropped with a warning.
- A restart forgets each session's open and filled notional and every symbol's last fill price.
  Credit counts fills since the later of the trading day's start and the gateway's, and orders
  left working at the venues across a restart count for nothing. A restart lifts every block.
- A client's request is committed by its session before the report answering it is stored by
  the client's session: a stop in between loses the report.
- A client's order, cancel or replace counts as received by its session once the venue's session
  has stored what it was forwarded as. A stop after the venue's session stored it but before the
  client's session recorded that has the client resend it, and with the book gone, a new order
  goes to the venue again under a new ClOrdID: the venue can get it twice. (A resent cancel or
  replace names an order the empty book doesn't know, and is rejected.)
- A client that reconnects while its venue is still storing a request has the resent request
  answered from the book, and counted as received at once: a stop before the venue stores it
  loses it. A venue store that fails releases the request too, stored or not.

## Database and recovery

- Without `[database] retention` the database is never pruned: done orders, the requests sent
  and ClOrdIDs used stay in it as history, so it grows for as long as the gateway trades. A start
  loads only live orders and each client's current day.
- Pruned orders are gone from the database for good, with their requests sent and ClOrdIDs.
- Done orders with no trading day (booked before their client's first day began) are never
  pruned, and neither is the table of orphans handled, which is small and grows only by crashes.
- Orders become prunable a trading day at a time, when their day ended, and their last request
  was sent, more than the retention ago, and are deleted at the next hourly run: up to an hour
  late.
- A run is skipped while work a start found for the venues is pending: orphan cancels waiting or
  being sent, orphaned replaces waiting, or unsent requests waiting for a venue's logon. A venue
  that never logs on again holds pruning off, with a warning each hour naming what's pending.
- With pruning on, the startup orphan scan takes a stored request of an order the database no
  longer has, sent no later than the latest pruned request plus a minute, as a pruned order's
  rather than an orphan. That relies on the gateway's clock not stepping forward and then back
  across a prune: a request sent in the stepped-back time could be taken as pruned, and its order
  not cancelled at the venue.
- Reports go to clients as the book makes them, before the change behind them commits. A fill
  whose change hadn't committed when the gateway crashed is redelivered by the venue after the
  restart and reaches the client again, with the same ExecID and PossResend(97)=Y: a client should
  tell fills apart by ExecID, so it shouldn't discard a PossResend report whose ExecID it hasn't
  seen. A new report the venue sends while its session recovers may be
  marked PossResend too, under a new ExecID.
- A request the venue's session dropped unsent (the client told Rejected, "venue unavailable")
  can still reach the venue if the gateway crashes before the reject commits: the restart sends
  it, and the client then hears New after Rejected.
- A write the database refuses every time (a full disk, say) is retried for as long as the
  gateway runs, at most a second apart, with an error logged each time. Every session with a
  message held behind it stops reading 256 messages past it, new orders are rejected as
  "gateway unavailable", operator calls don't return, and shutdown waits for it too: a second
  Ctrl-C or SIGTERM exits.
- Messages handled in the last commit before a clean shutdown are redelivered, flagged as
  possible resends, at the next start: the sessions had closed before they could record them
  as received. The book takes them as repeats.
- Each time a venue's session reconnects it asks the venue to resend from its oldest message
  not yet committed; the reports it has already handled are dropped as stale.
- Each start reads every message in every venue's session store, to compare its requests with
  the database: about 3.9 s per million stored messages, measured from an in-memory store; a
  disk store adds its read time. A large store makes for a slow start.
- A reload that keeps a client's or venue's session in memory is applied all the same, with an
  error logged; the next start refuses the file.
- A client that logs on with ResetSeqNumFlag(141)=Y after a restart doesn't redeliver the
  messages that hadn't committed before the stop, and loses the reports its session had stored
  but not yet sent.
- Deleting a venue's disk store while the database has orders for that venue stops every start,
  since the gateway can no longer tell what the venue was sent. To start a venue's sequence
  numbers again, reset them rather than delete its files: pause the venue's session, then
  `POST /api/sessions/{id}/reset` (or `reset_sequence_numbers()` on `Service::venue_session(name)`
  while it's disconnected).
- An orphan (a request the venue's session stored that the database never recorded, from a crash
  within a commit of sending it) is logged as an error at the start. An orphaned order is
  cancelled at the venue's next logon, under a ClOrdID of the gateway's own (`<prefix>C<n>`);
  the venue's answers to that cancel, and any fills on the order, are logged as errors and reach
  no client, so check the logs for fills before the cancel. The client's resent order is booked
  and sent afresh, so the venue sees the order twice, the first cancelled. A cancel the venue's
  session drops goes again at the next logon.
- An orphaned cancel has its order cancelled by the gateway. An orphaned replace stays pending on
  its order, which keeps working; the client's redelivered replace with the same terms is taken
  as it, and the venue's acceptance reaches the client as Replaced:
  - a replace that isn't marked as a possible resend (PossDupFlag or PossResend) isn't taken as
    the orphan: the venue's acceptance reaches the client as Restated, with the new terms;
  - a redelivered replace on other terms is refused as "replace pending" while the orphan is;
  - one taken while the orphan waits for an earlier replace to be answered isn't stored: a
    restart before the earlier one is answered ends in Restated, not Replaced;
  - one taken after the order has ended is answered Replaced, with the order's final status
    (Canceled, say).

  In each case the venue's order has the terms of the last replace it accepted; the logs say
  which.
- A request the database has as sent, unanswered, that the venue's session never stored (a
  crash after the commit, before the store) is sent at the venue's next logon, marked
  PossResend(97). Some aren't:
  - one sent before the venue's store was created or last reset is left pending, logged: the
    venue had it;
  - if the venue's session resets its sequence numbers at that logon (a new schedule period, or
    `reset_on_logon`), each fails instead, and a client's gets a reject. The reset is told from the
    session's sequence numbers, so one whose store was empty at the start can't be told, and the
    requests are sent;
  - a store with a size budget that has evicted an unanswered request since its last reset
    makes it look unsent: it goes again, and the venue may refuse it as a duplicate.

## Trading day

- A venue's report on an order done and dropped at the end of its client's day (a late
  correction, a resend) is for an unknown order: it's dropped with a warning, and the client
  doesn't hear of it. So is a cancel reject for one.
- A day rolls over at the session's next message, or the next tick, once a second: a quiet
  session's done orders and fills count up to a second past its end. A day with a schedule
  ends at the earlier of `day_end` and the period's end, so a daily schedule's sessions also roll
  at `day_end` while they're logged out, which drops nothing they'd miss.
- Days follow the gateway's clock. A clock stepped forward past a day's end rolls the day; stepped
  back again, the session stays in that later day until it ends, and the days it skipped never
  start: a day earlier than a session's own is ignored.
- A live order carries every ClOrdID it has used into the next day, its original's and those of
  every replace and cancel since, and they count against the new day's 1,000,000.
- The last fill price a Market order or the price band works from is a symbol's last fill on any
  day, however old.
- A changed `day_end` or schedule applies from each session's next day: the day under way ends
  when it was going to, even if the new setting puts its end earlier or later. Turbojet applies a
  changed schedule at the session's next logon, so for a while the gateway's day and the
  session's hours can follow different schedules.
- A possible resend of an order the gateway doesn't know is booked if it doesn't say when it was
  first sent, or was first sent since its session's day began; one first sent before is rejected
  as a duplicate. In a session's first day (since the gateway started, without a database), every
  such resend is booked.
- The roll walks the whole book, under the book's lock, once for all the sessions whose day ends
  at the same tick, or once for a session rolled by its own message: a large book holds the lock
  for a while.

## Orders and reports

- Trade busts and corrections (ExecType TradeCancel or TradeCorrect, and ExecTransType Cancel or
  Correct on FIX 4.2) are logged as errors and dropped: the client doesn't hear of them, and the
  order's CumQty stays as it was.
- A venue fill without LastPx(31) is refused with a session-level Reject and logged as an error;
  the book doesn't see it.
- A report that takes CumQty past the order's quantity is logged as a warning and dropped.
- A fill the gateway missed (a Status report raising CumQty without LastQty(32), or a fill
  raising it by more than its LastQty) reaches the client as a Trade for the missing quantity,
  priced from the change in AvgPx when the venue gave no LastPx for it: approximate, by the
  venue's AvgPx rounding.
- After a replace is accepted, reports without a fill under a ClOrdID it superseded are dropped
  (fills and done statuses still apply), so a venue that keeps reporting under the original
  ClOrdID loses its restatements. A dropped restatement is logged as a warning.
- A cancel sent while a replace is pending names the last ClOrdID the venue accepted. A strict
  venue that has just accepted the replace may reject the cancel as for an unknown order; the
  client gets the reject and can cancel again.
- OrigClOrdID is resolved loosely: any ClOrdID the client has used for an order (the original, a
  superseded one, a cancel's) names that order.
- A replace changes quantity, price, OrdType and TimeInForce; the rest stays the original's. A
  replace without TimeInForce makes the order Day.
- Rejected orders' ClOrdIDs aren't remembered, so a client can reuse one and the order is booked.
  A routes reload between an order and its resend could book an order the client saw rejected.
- ExecTypes the gateway doesn't model (Suspended, Calculated, Stopped, a venue's own codes)
  reach the client as Restated, and OrdStatuses it doesn't model leave the order's status as it
  was, each with a warning logged. DoneForDay passes through and leaves the order open.
- A client receives its own reports in the order the gateway made them. Reports to different
  clients aren't ordered with each other.

## Risk limits

- The reference price for a Market order's notional and for the price band is the symbol's last
  fill on any venue, for any client, as the venues reported it. There's no market data: a symbol
  that hasn't filled (since start, without a database) has no reference, so the band doesn't
  apply to it and a Market order in it is rejected where `max_notional` or `credit` applies. A
  fill price of zero or less isn't taken as a reference.
- A Market order booked without a reference (where no notional or credit limit applied) counts
  as zero notional for as long as it's live: a credit limit added later, or a reference that
  appears, doesn't re-price it. A Market order's notional is fixed at the reference when it's
  booked, and doesn't move with later fills.
- The price band applies to either side: a passive order priced far from the last fill, a buy
  well below it say, is refused as well as an aggressive one.
- A CompID's table can't unset a numeric default or the defaults' `symbols`; set a large number,
  or list the symbols, instead.
- Limits are per session, not per CompID: a CompID with several sessions can have that many times
  the credit open. Self-trade prevention is per session too, and only within one venue: a buy on
  one venue and a sell on another for the same symbol both pass.
- Self-trade prevention compares prices only: an order with a pending replace rests at the more
  aggressive of its old and new prices, and a live Market order crosses anything on the other side
  until it's done.
- Each order's and each fill's notional counts at most 10^18 and is rounded to 4 decimal places,
  so a session's open and filled notional add up exactly.
- Filled notional counts the fills the book takes. An overfill it drops isn't counted; a trade
  bust or correction, also dropped, leaves the busted fill counted. A fill the gateway missed
  counts at the price the change in AvgPx implies, and doesn't move the reference price.
- A replace counts as the larger of the order's old and new terms until the venue answers it. A
  replace is checked against credit only when it raises the order's notional, so one that cuts the
  notional passes even over the credit limit; one at the same price (or Market again) with no more
  quantity skips every risk check.
- The last fill prices are kept per symbol for the gateway's life, across trading days (and
  restarts, with a database): one entry for each symbol traded.
- A reload's new limits apply to the next order or replace: orders already working aren't
  checked again or cancelled.

## Editing the limits over the API

- An edit rewrites `jetbridge.toml`, so the gateway's user must be able to write it and to create
  and rename a file in its directory (the directory of the file a symlink points to). The new
  file belongs to the gateway's user; an owner that changed, or a directory that couldn't be
  synced once the file was in place, comes back as a warning in the answer.
- An edit reformats only the table it touches. A comment after a value it replaces stays; a
  deleted table takes its comments with it, and any just before it. A new client's table goes at
  the end of the file.
- A file broken by hand refuses every edit (422) until it's fixed; a `DELETE` of a table that
  isn't there answers 404 before that, as long as the file is still valid TOML (one that doesn't
  parse answers 422 first). Limits written as `[risk]` keys or inline tables rather than
  `[risk.defaults]` and `[risk.client.COMPID]` tables answer 422: edit those by hand.
- An edit checks the routes against the venues running now, so a venue added by hand to both
  files but not yet reloaded makes every edit 422 until a reload starts it.
- Each edit runs a full reload, which takes whatever else was changed by hand in `jetbridge.toml`
  and the sessions file. A hand edit saved while an API edit is under way is overwritten.
- Removing a client's last key leaves its table, empty: the client has the defaults, but still a
  table until it's deleted.
- CompIDs the API refuses in a path (with dots, spaces, quotes, backslashes or non-ASCII) show in
  `GET /api/risk/limits` but can't be edited over the API.
- A JSON number that isn't an integer is written as a TOML float, which may not keep every digit:
  send an exact decimal as a string. An integer too large for TOML is written as a string.

## Venues

- FIX 4.2 venues get HandlInst(21) 1 (automated, private) on every order and replace, whatever
  the client sent; FIX 4.4 venues get none.
- Only Account, ExDestination, Symbol, Side, OrderQty, Price, OrdType and TimeInForce reach the
  venue, Account and ExDestination as the client sent them: nothing is mapped per venue, and
  other fields the client sent are dropped.
- A venue's logon is refused until the gateway knows the venue: at start, and after a reload adds
  one, its initiator can log on before then, and its reconnect policy retries.
- A venue request its session's store fails on may or may not have been stored: it's logged as
  an error and the order or request stays pending, with nothing sent to the client.
- A venue a reload removes leaves its open orders in the book: cancels and replaces for them are
  refused as venue unavailable, and its reports, should its session still be up, are rejected.
- Jetbridge is built without Turbojet's `validation` feature, so a `dictionary` in the sessions
  file doesn't load. On a venue initiator, one would refuse reports with a venue's own codes
  before the gateway saw them.

## Clients

- A client is a session. Sessions of one CompID told apart by SubID or LocationID are separate
  clients, each with its own ClOrdIDs and reports; routes match their CompID, so they route alike.
- Every client speaks the acceptor's BeginString.
- A CompID may log on with up to 16 sessions; a logon on a 17th is refused. A logon whose SubID
  or LocationID contains '/' is refused too, since it could print the same as another session.
- Each session that logs on keeps a report queue and its task until a restart, and each that
  orders its trading day in the book and the database. A restart counts, against the 16, only the
  sessions with live orders, undelivered reports or a block of their own. Listing the
  counterparties bounds the CompIDs, and so the sessions; under `unknown = "admit"` they aren't
  bounded.
- The ClOrdID budget (1,000,000 a day) is per session, so one CompID can use up to 16 times it.
- A client holds up to 100,000 undelivered reports. The first past that is dropped, with an
  error logged, and the client is logged out; later ones are dropped and counted until its queue
  empties.
- Cancel on disconnect countdowns still pending when the gateway shuts down fire then, with the
  clients logged out; the shutdown's own Logouts don't start any. Instead the gateway cancels the
  working orders of every client session whose CompID has cancel on disconnect on, whatever its
  trigger (a `disconnect` client's too), as the sessions file in use says now, not as it was at the
  client's logon; other clients' orders stay working at their venues. Before logging the venues
  out, shutdown waits up to 5 s for them to answer all the gateway's cancels (the kill switch's
  too), and the clients don't receive those Canceled reports before the stop (with a database, they
  get them, marked PossResend, when they log on after a restart). A venue that's down with
  cancels waiting for it costs the full 5 s. A process that stops without shutting down loses
  its countdowns: a restart doesn't start them again.
- A panic in the book stops the process rather than carry on with a book that may be half
  changed.

## Operator API

- Logins are kept in memory: a restart logs everyone out. At most 1,024 are kept; past that, the
  least recently used ends.
- Keep a token for the kill switch. Anyone who can reach the API can lock a user name out of
  password login for a minute with five wrong passwords, and a flood of failed logins from 10,000
  names, or 10,000 addresses, blocks password logins from new ones until they age out. Tokens
  aren't limited.
- The blotter (`GET /api/orders`, `GET /api/orders/{order_id}`) reads the book: working orders and
  each client's done orders of its current trading day. Earlier days are in the database, if there
  is one, but not readable over the API.
- `GET /api/sessions` lists a client session only while it's connected or paused, so a client
  that has never connected, or disconnected without being paused, isn't listed and can't be
  paused (404). Pausing logs the session out: it shows as disconnected and paused.
- `GET /api/audit` reads only the days it needs, but each of those days' files whole. Without
  `audit_retention` the daily files are kept for ever. A file moved away is created afresh at
  the next line, and the old one's entries are no longer read back.
  Lines are written one at a time within the gateway's process only: two processes mustn't share
  an audit directory. A change waits for its line behind any refusals being written, up to 120 a
  minute of anonymous ones, each synced to disk.
  Refusals by identified callers aren't budgeted: a token holder posting refused mutations in a
  loop writes one synced line each, and can delay real mutations.
- A reload answered 422 may have applied half: the file that loaded is in use. The answer says
  which.
- `jetbridge user` replaces a dangling symlink at the users file's path with a regular file.
- Setting a session's sequence numbers while it's logged on has effects on the counterparty;
  pausing it first is safer:
  - lowering next_incoming can have messages accepted again, or, with messages in flight, draw a
    ResendRequest or a Logout. On a client's session, messages accepted again are answered by the
    book as duplicates, by ClOrdID;
  - raising a client's next_incoming skips messages: an order among them is lost;
  - next_outgoing only moves forward, and a logged-on counterparty is told with a gap fill.
- A reset only works on a disconnected session, and clears its resend store: the messages sent
  before it can't be sent again. For a reset the counterparty agrees to, it logs on with
  ResetSeqNumFlag(141)=Y.
- Given both numbers, the outgoing one is set first, so a refusal changes nothing; only a store
  failure setting the incoming one leaves the outgoing one set, and the error and the audit line
  say so.
- A status request to a venue that isn't logged on is refused (409), not queued: ask again once
  it is. It's sent under the gateway's ClOrdID the venue last acknowledged, not a pending
  replace's.
- An answer that changes the order reaches the client as an unsolicited report with ExecType
  Status, a missed fill as a Trade.
- Only an ExecutionReport with ExecType Status, OrdStatus Rejected and OrdRejReason(103) 5 (unknown
  order) is recognised as a venue not knowing an order; a BusinessMessageReject or an
  OrderCancelReject isn't. That report is taken so whether the gateway asked or not: it no longer
  rejects the order, or refuses a cancel or replace pending on it.
- After an `unknown_at_venue` alarm the order stays live, its exposure counted, until an operator
  settles it with the venue and resolves it (`POST /api/orders/{order_id}/resolve`). "Unknown"
  under a ClOrdID a replace has since superseded is only a warning in the log: ask again.
- The mark that lets an order be resolved is kept in memory only: after a restart, ask the venue
  again before resolving.
- A resolved order's client hears only the Canceled report: a cancel or replace of its own still
  outstanding on the order isn't answered separately.
- A fill a venue reports on an order the book has as done, after a resolve or crossing the
  venue's own cancel, isn't applied and doesn't reach the client: it raises a
  `fill_on_done_order` alarm, and someone has to settle it with the venue.
- The alarm isn't deduplicated: the same "unknown order" answer redelivered (PossDupFlag, after a
  resend) raises it again.
- Each API connection holds one of 256 slots for as long as it's open; one that moves no bytes for
  60 seconds is closed, so an idle connection holds its slot for up to that long. Past 256, new
  connections wait in the kernel's backlog. A live socket holds one too, up to 64 of them, and its
  pings keep it open. A request whose handler moves no bytes for 60 seconds is cut off with its
  connection.
- Alarms are sent only to the live sockets open when they're raised; earlier ones are in the log.
  The alarms of a start's recovery (orphans) come before the API is up, so only the log has them.
- Behind a proxy, the client's address is whatever the last `X-Forwarded-For` hop says, and a
  page's host whatever the last `X-Forwarded-Host` says: a proxy that passes the client's own
  through lets it choose its address for the login limit and the audit log. The proxy must set
  and overwrite them. A proxy that sends no `X-Forwarded-For` is every client's address, with a
  warning logged once: every login then shares one limit.
- Under TLS, or behind a proxy, the session cookie is `__Host-jetbridge_session` with `Path=/`, so
  the browser sends it to every path on the API's host name: give the API a host name of its own,
  not a path on a site that serves other pages.
- A live socket's snapshot holds up to 1,000 working orders; the rest are read from
  `GET /api/orders`, and may change between the two.

## Message log

- Messages are written with no `fsync`: they survive the gateway's process crashing, not the
  machine. If the log's 16 MiB buffer fills, messages are dropped, and the gap recorded in the
  file as a `dropped` line.
- A client's messages before its Logon names its session are logged with session `-` (`null` in a
  search), so a search by `session` doesn't find them.
- A file is finished, and compressed, only when the next one starts or the gateway stops.
- The log is opened before the sessions file is loaded: a start that stops on a bad sessions
  file leaves an empty log file behind.
- A search reads the files with no index, so a rare match on a busy day pages slowly, up to
  256 MiB a page. Resuming in a `.log.gz` file decompresses it up to the cursor. A search stops
  reading a file at a corrupt record, and the rest of that file isn't searched.
- A cursor is refused once retention has deleted its file, or if the record it points at is
  corrupt. `follow` assumes one gateway writes to the log's directory.
- A build without the `gzip` feature skips `.log.gz` files in a search, with a warning logged.
- An order's messages are searched for a day from its creation: later ones, a GTC order's say,
  are found with `GET /api/messages` by tag. A done order from before its client's current
  trading day isn't in the book, so it has no `{order_id}`: search for it by tag too. A venue
  whose initiator a reload stopped has no session to search, so only the client's side of its
  orders is found.
