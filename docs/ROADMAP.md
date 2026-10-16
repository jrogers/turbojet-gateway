# Roadmap

What's left to do on Turbojet Hub, in rough priority order. Each item says why it matters and roughly
how big it is: **S** (a day or less), **M** (a few days), **L** (a week or more). Behaviour worth
knowing about is in [CAVEATS.md](CAVEATS.md).

## 1. Risk and routing

- **Market data for price bands** (M). The band's reference is the last fill on the symbol; a
  market data feed would give a better one.

## 2. Scale

- **Split the book** (L). One lock guards the book, router and risk: the known ceiling. Splitting
  by client would lift it if it ever binds.
