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
- A client's order, cancel or replace counts as received by its session only once the venue's
  session has stored what it was forwarded as. A stop after the venue's session stored it but
  before the client's session recorded that has the client resend it, and with the book gone, it
  goes to the venue again under a new ClOrdID: the venue can get it twice.
- A venue session that's slow to store, or stuck, holds back the clients whose requests it hasn't
  stored: 256 messages past the oldest one waiting, a client's session stops reading from it until
  the venue's session catches up. Turbojet logs a warning when that happens. Its
  `turbojet_held_back` gauge needs Turbojet's `metrics` feature, which Jetbridge isn't built with.
- Operators' blocks (see the README's kill switch) are kept in memory: a restart lifts them.
- So is what the risk limits count: a restart forgets each session's open and filled notional and
  every symbol's last fill price. Credit counts fills since the later of the trading day's start
  and the gateway's, and orders left working at the venues across a restart count for nothing.
- A gateway cancel (the kill switch's) that a venue's session can't queue is tried again once a
  second while the session is connected, without limit.
- At the end of a session's trading day its done orders, their ClOrdIDs and requests sent, and
  its filled notional are dropped (see the README's trading day). Live orders carry over, GTC
  ones for as long as they work, and so do last fill prices, one per symbol traded. The bound of
  1,000,000 ClOrdIDs per client (plus 10,000 for cancels and replaces once it's reached) counts
  per day, from the ClOrdIDs of the live orders carried over.

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
  as a duplicate. In a session's first day since the gateway started, every such resend is
  booked.
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
  that hasn't filled since start has no reference, so the band doesn't apply to it and a Market
  order in it is rejected where `max_notional` or `credit` applies. A fill price of zero or less
  isn't taken as a reference.
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
- The last fill prices are kept per symbol for the gateway's life, across trading days: one entry
  for each symbol traded.
- A reload's new limits apply to the next order or replace: orders already working aren't
  checked again or cancelled.

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
- Each session that logs on keeps a report queue and its task, and each that orders its trading
  day in the book, until a restart. Listing the counterparties bounds the CompIDs, and so the sessions;
  under `unknown = "admit"` they aren't bounded.
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
  too), and the clients don't receive those Canceled reports. A venue that's down with cancels
  waiting for it costs the full 5 s. A process that stops without shutting down loses its
  countdowns, as it loses the book.
- A panic in the book stops the process rather than carry on with a book that may be half
  changed.
