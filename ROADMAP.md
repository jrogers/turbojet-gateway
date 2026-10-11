# Roadmap

What's left to do on Jetbridge, in rough priority order. Each item says why it matters and roughly
how big it is: **S** (a day or less), **M** (a few days), **L** (a week or more). Behaviour worth
knowing about is in [CAVEATS.md](CAVEATS.md).

## 1. Operator access

Operator actions (block, unblock, cancel all) are library calls on the gateway today.

- **HTTP API** (L). JSON over axum on its own port, with a websocket that sends a snapshot of
  sessions and working orders and then events. Covers sessions (status, pause, resume, sequence
  numbers), the blotter and one order's messages, risk limits and usage, the kill switch, routing
  rules with a dry run, config reload, and message log search. Needs:
  - scopes that parse and print, a client looked up by its session string, and an order cancelled
    by its OrderID or by (session, ClOrdID);
  - read access to risk state (each session's open and filled notional, reference prices);
  - an operator path for stuck gateway cancels;
  - `block` and `unblock` returning the same shape.
- **Users, roles and audit** (M). Local users in `users.toml` with argon2 password hashes, API
  tokens stored hashed, and three roles (viewer, operator, admin), scoped by CompID for blocks and
  cancels. Every operator call carries who made it and why, and goes to an append-only audit log
  before it's answered, refusals included.
- **Console** (L). A React and TypeScript web console in `web/`, built into release builds:
  sessions, blotter, risk with the kill switch, logs, and admin (routing, reload, audit).

## 2. Durability

- **PostgreSQL** (M). The store's SQL is portable; it needs the driver behind a feature, passwords
  masked properly in connect errors (including a `password` query parameter), the `?mode=rwc` hint
  only for SQLite URLs, and the startup load under REPEATABLE READ.
- **Pruning history** (S). Done orders are never deleted, so the database grows without limit.
- **Refuse memory session stores at the first start** (S). With a database, client and venue
  sessions both need a durable store. Today a memory venue store is refused only once the
  database holds orders for that venue, and a client's isn't checked. Needs Turbojet to say
  whether a store is durable.
- **Orphan scan checkpoint** (M). Each start reads every venue store whole, about 3.9 s per
  million stored messages; a checkpoint would read only what's past the last commit.
- **Orphaned replace edge cases** (S). The cases listed in CAVEATS where a client hears Restated
  or a refusal for a replace the venue accepted.

## 3. Risk and routing

- **Market data for price bands** (M). The band's reference is the last fill on the symbol; a
  market data feed would give a better one.

## 4. Scale

- **Split the book** (L). One lock guards the book, router and risk: the known ceiling. Splitting
  by client would lift it if it ever binds.
