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

## 4. Tooling

- **Demo replace and status** (S). `scripts/fix-demo.py`'s client sends orders and a cancel, but
  no OrderCancelReplaceRequest or OrderStatusRequest, so a demo never shows a replace or a
  status request.
