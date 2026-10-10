# Known caveats

Behaviour that is deliberate or documented, but worth knowing about before you rely on Jetbridge.
Turbojet's own are in its [CAVEATS.md](https://github.com/jrogers/turbojet/blob/main/CAVEATS.md).

## Kept in memory

- The book (orders, the ClOrdIDs each client has used, what was sent to each venue) and the
  reports held for clients live in memory. A stop loses them; shutting down logs how many open
  orders and undelivered reports it leaves. Open orders stay working at their venues.
- After a restart, numbering starts again at 1, but every ID the gateway issues (OrderIDs,
  ExecIDs and the ClOrdIDs sent to venues) starts with a prefix from the start time: six base-36
  digits of the seconds since 2026, such as `0EI800` in `0EI8001-1`. A late report from a venue
  on an order from before the restart names an unknown order and is dropped with a warning. Two
  starts within the same second share a prefix, and so repeat IDs. The prefix takes six of a
  venue's ClOrdID length limit.
- A client's request is committed by its session before the report answering it is stored by
  the client's session: a stop in between loses the report.
- Operators' blocks (see the README's kill switch) are kept in memory: a restart lifts them.
- A gateway cancel (the kill switch's) that a venue's session can't queue is tried again once a
  second while the session is connected, without limit.
- Nothing is cleared at the end of the day: orders, ClOrdIDs and requests sent stay in memory
  until a restart. The bound of 1,000,000 ClOrdIDs per client (plus 10,000 for cancels and
  replaces once it's reached) counts from start, not per day.

## Orders and reports

- Trade busts and corrections (ExecType TradeCancel or TradeCorrect, and ExecTransType Cancel or
  Correct on FIX 4.2) are logged as errors and dropped: the client doesn't hear of them, and the
  order's CumQty stays as it was.
- A venue fill without LastPx(31) is refused with a session-level Reject and logged as an error;
  the book doesn't see it.
- A report that takes CumQty past the order's quantity is logged as a warning and dropped.
- A venue Status report carrying a fill the gateway missed updates CumQty without a Trade to the
  client: the client sees the new CumQty on a non-fill report.
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
- Each session that logs on keeps a report queue and its task, and each that orders a count in
  the book, until a restart. Listing the counterparties bounds the CompIDs, and so the sessions;
  under `unknown = "admit"` they aren't bounded.
- The ClOrdID budget (1,000,000 a day) is per session, so one CompID can use up to 16 times it.
- A client holds up to 100,000 undelivered reports. The first past that is dropped, with an
  error logged, and the client is logged out; later ones are dropped and counted until its queue
  empties.
- Cancel on disconnect countdowns still pending when the gateway shuts down fire then (and under
  `disconnect_or_logout`, the shutdown's own Logouts start them), but the venues are logging out
  at the same time, so those cancels may not reach them; the shutdown log lists them. A process
  that stops without shutting down loses its countdowns, as it loses the book until stage 4.
- There are no risk checks: every order a route sends to a venue goes there.
- A panic in the book stops the process rather than carry on with a book that may be half
  changed.
