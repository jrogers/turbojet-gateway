# Roadmap

What's left to do on Turbojet Gateway, in rough priority order. Each item says why it matters and
roughly how big it is: **S** (a day or less), **M** (a few days), **L** (a week or more). Behaviour
worth knowing about is in [CAVEATS.md](CAVEATS.md).

## 1. Orders and routing

- **Strict ClOrdID history** (S). Any ClOrdID a client has used for an order, the original or a
  superseded one, names that order as an OrigClOrdID. A strict venue refuses all but the latest,
  and a client certified against one may expect the gateway to do the same.
- **Allocations** (S). NoAllocs isn't handled: a client's allocations don't reach the venue, and
  nothing checks that their quantities add up to the OrderQty.
- **Dialect mapping** (L). Orders translate between FIX 4.2, 4.4 and 5.0 SP2, but not between
  counterparties' dialects. Rules per venue or client to set, drop or map fields, as hubs like FIX
  Antenna's FIXEdge do, would let one client's orders meet each venue's rules of engagement.

## 2. Risk

- **Market data for price bands** (M). The band's reference is the last fill on the symbol; a
  market data feed would give a better one.

## 3. Scale and availability

- **High availability** (L). A hot standby gateway that takes over the book and every session on
  failover, with no gap and no resend storm. Today a gateway down is trading down, and nothing
  stops two gateways sharing one database from overwriting each other's book. It needs an
  exclusive lease on the book, deciding which instance is active, and Turbojet replicating each
  session's journal to the standby (on Turbojet's roadmap).
- **Split the book** (L). One lock guards the book, router and risk: the known ceiling. Splitting
  by client would lift it if it ever binds.

## 4. Operations

- **Structured logging** (S). Logs come only in `tracing`'s text format (`src/main.rs`), and the
  gateway's own events (an order routed or refused, a risk limit hit, a reload) put what they're
  about in the text. A `[log]` setting for JSON output, with those events carrying the client,
  venue, ClOrdID and symbol as fields and a stable name each, would let a log pipeline filter and
  alert on them without parsing.
- **OpenTelemetry** (M). The gateway exports no metrics or traces. Behind a feature, configured
  in a `[telemetry]` table (the OTLP endpoint and sampling):
  - a trace per order, from the client's NewOrderSingle through risk and routing to the venue and
    back to each execution report, with its ClOrdIDs as attributes, so a slow or lost order can
    be followed across both sessions;
  - metrics over OTLP: Turbojet's per-session ones (its `metrics` feature) and the gateway's own,
    such as orders and rejects by reason, risk limits hit and open orders by venue.
