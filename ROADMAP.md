# Roadmap

What's left to do on Jetbridge, in rough priority order. Each item says why it matters and roughly
how big it is: **S** (a day or less), **M** (a few days), **L** (a week or more). Behaviour worth
knowing about is in [CAVEATS.md](CAVEATS.md).

## 1. Operator access

The [API](README.md#api) covers sessions and their sequence numbers, the blotter, asking a venue
for an order's status and resolving one it doesn't know, risk with the kill switch and limit
editing, routes with a dry run, the message log with search, reload and the audit log. Still to
do:

- **History beyond the current day** (M). The blotter reads the book, which holds each client's
  current day only; earlier days are in the database but not readable over the API.
- **Console** (L). A React and TypeScript web console in `web/`, built into release builds:
  sessions, blotter, risk with the kill switch, logs, and admin (routing, reload, audit).

## 2. Durability

- **PostgreSQL** (M). The store's SQL is portable; it needs the driver behind a feature, passwords
  masked properly in connect errors (including a `password` query parameter), the `?mode=rwc` hint
  only for SQLite URLs, and the startup load under REPEATABLE READ.
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
