# Roadmap

What's left to do on Jetbridge, in rough priority order. Each item says why it matters and roughly
how big it is: **S** (a day or less), **M** (a few days), **L** (a week or more). Behaviour worth
knowing about is in [CAVEATS.md](CAVEATS.md).

## 1. Operator access

The [API](README.md#api) and the [console](README.md#console) cover sessions and their sequence
numbers, the blotter and past days' orders, asking a venue for an order's status and resolving one
it doesn't know, risk with the kill switch and limit editing, routes with a dry run, the message
log with search, reload and the audit log. Nothing is open here; where they fall short is in
[CAVEATS.md](CAVEATS.md#operator-api).

## 2. Durability

- **PostgreSQL** (M). The store's SQL is portable; it needs the driver behind a feature, passwords
  masked properly in connect errors (including a `password` query parameter), the `?mode=rwc` hint
  only for SQLite URLs, the startup load under REPEATABLE READ, and its history read pool made
  read-only (`SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` on connect).
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
