//! Pre-trade risk: the limits each client's orders are checked against before they're routed,
//! and the [`State`] the book keeps for the checks. [`check`] is pure: the book calls it and
//! keeps the state up to date.

use std::collections::HashMap;

use turbojet::fields::{CompactString, Decimal};

use crate::book::{ClientId, OrderId, Side, VenueId};

/// Decimal places notionals are counted to: with [`MAX_ORDER_NOTIONAL`], what keeps a session's
/// open notional exact as orders come and go (see [`notional`]).
pub const NOTIONAL_DP: u32 = 4;

/// One order's notional counts at most this, 10^18. A session's open notional sums at most
/// [`MAX_CL_ORD_IDS_PER_CLIENT`](crate::book::MAX_CL_ORD_IDS_PER_CLIENT) +
/// [`MAX_AMEND_RESERVE`](crate::book::MAX_AMEND_RESERVE) orders' (a replace books none), about
/// 1.01 x 10^24, which at four places is 1.01 x 10^28 units: under a Decimal's 7.9 x 10^28, so
/// the sum is exact and taking an order back out leaves what was there before.
pub const MAX_ORDER_NOTIONAL: Decimal = Decimal::from_parts(0xA764_0000, 0x0DE0_B6B3, 0, false, 0);

/// Why risk refused an order or replace; the FIX side maps it to OrdRejReason(103).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskReject {
    Symbol,
    OrderQty,
    /// A Market order with no reference price, where a notional or credit limit needs one.
    NoReferencePrice,
    Notional,
    PriceBand,
    SelfTrade,
    Credit,
}

impl RiskReject {
    /// Text(58) for the client, naming the limit.
    pub fn text(self) -> &'static str {
        match self {
            Self::Symbol => "symbol not allowed (risk symbols)",
            Self::OrderQty => "quantity over the limit (risk max_order_qty)",
            Self::NoReferencePrice => "no reference price for a market order (risk max_notional or credit)",
            Self::Notional => "notional over the limit (risk max_notional)",
            Self::PriceBand => "price outside the band around the last fill (risk price_band)",
            Self::SelfTrade => "would trade with the session's own resting order (risk self_trade)",
            Self::Credit => "over the credit limit (risk credit)",
        }
    }
}

/// An order, or a replace's new terms, as the checks see it.
#[derive(Debug, Clone, Copy)]
pub struct Proposed<'a> {
    pub client: &'a ClientId,
    pub symbol: &'a str,
    pub venue: &'a str,
    pub side: Side,
    /// OrderQty(38).
    pub qty: Decimal,
    /// What's left to fill: the quantity, less what a replaced order has filled.
    pub leaves: Decimal,
    /// The limit price; `None` for a Market order.
    pub price: Option<Decimal>,
    /// The order a replace is for, whose exposure the new terms replace.
    pub replaces: Option<OrderId>,
}

/// Whether `order` is within `limits`, given what `state` says the session has open and filled.
/// A replace is checked on its new terms, and against credit only if it raises the order's
/// notional: one that takes risk off passes.
///
/// # Errors
///
/// The first limit the order breaks, in the order of [`RiskReject`]'s variants.
pub fn check(limits: &Limits, order: &Proposed, state: &State) -> Result<(), RiskReject> {
    if let Some(symbols) = &limits.symbols
        && !symbols.iter().any(|symbol| symbol == order.symbol)
    {
        return Err(RiskReject::Symbol);
    }
    if limits.max_order_qty.is_some_and(|max| order.qty > max) {
        return Err(RiskReject::OrderQty);
    }
    let reference = state.reference(order.symbol);
    let price = order.price.or(reference);
    if price.is_none() && (limits.max_notional.is_some() || limits.credit.is_some()) {
        return Err(RiskReject::NoReferencePrice);
    }
    let price = price.unwrap_or(Decimal::ZERO);
    if limits.max_notional.is_some_and(|max| notional(order.qty, price) > max) {
        return Err(RiskReject::Notional);
    }
    if let (Some(band), Some(limit), Some(reference)) = (limits.price_band, order.price, reference)
        && !within_band(limit, reference, band)
    {
        return Err(RiskReject::PriceBand);
    }
    if limits.self_trade && state.crosses_own(order) {
        return Err(RiskReject::SelfTrade);
    }
    if let Some(credit) = limits.credit
        && !state.within_credit(order, notional(order.leaves, price), credit)
    {
        return Err(RiskReject::Credit);
    }
    Ok(())
}

/// What `qty` at `price` counts for: positive, at most [`MAX_ORDER_NOTIONAL`], to
/// [`NOTIONAL_DP`] places.
pub fn notional(qty: Decimal, price: Decimal) -> Decimal {
    qty.checked_mul(price)
        .map_or(MAX_ORDER_NOTIONAL, |notional| notional.abs().min(MAX_ORDER_NOTIONAL))
        .round_dp(NOTIONAL_DP)
}

/// `price` within `band` percent either side of `reference`. Amounts too big to compute are
/// outside.
fn within_band(price: Decimal, reference: Decimal, band: Decimal) -> bool {
    let width = reference.abs().checked_mul(band).and_then(|width| width.checked_div(Decimal::ONE_HUNDRED));
    let distance = price.checked_sub(reference).map(|distance| distance.abs());
    matches!((distance, width), (Some(distance), Some(width)) if distance <= width)
}

/// What a live order counts for: in its session's open notional, and, resting, against the
/// session's orders on the other side for self-trade prevention. The book makes one from each
/// live order after every change to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    pub client: ClientId,
    pub symbol: CompactString,
    pub venue: VenueId,
    pub side: Side,
    /// The limit price; `None` for a Market order, which crosses any order on the other side.
    pub price: Option<Decimal>,
    /// Leaves at the order's risk price (its limit, or a Market order's reference when it was
    /// booked), or the pending replace's, if more.
    pub notional: Decimal,
}

/// What the checks need to know, kept up to date by the book: reference prices, and each
/// session's open and filled notional and resting orders.
#[derive(Debug, Default)]
pub struct State {
    /// The last fill's price per symbol, on any venue, for any client. Bounded by the book's
    /// orders: only a fill on a booked order inserts one, and the book keeps its orders.
    references: HashMap<CompactString, Decimal>,
    sessions: HashMap<ClientId, Session>,
    /// Each live order's exposure, as last tracked.
    exposures: HashMap<OrderId, Exposure>,
}

#[derive(Debug, Default)]
struct Session {
    /// The sum of the session's live orders' exposures.
    open: Decimal,
    /// Filled since start: "today" until stage 4 keeps the book across restarts.
    filled: Decimal,
    /// Live orders by (symbol, venue, buying), with their limit prices.
    // ponytail: a list per key, scanned per order; a price-ordered map if a session rests many
    // orders on one symbol and venue.
    resting: HashMap<RestingKey, Vec<(OrderId, Option<Decimal>)>>,
}

/// Symbol, venue and whether the orders are buying (SellShort sells).
type RestingKey = (CompactString, VenueId, bool);

fn buying(side: Side) -> bool {
    side == Side::Buy
}

impl State {
    /// The symbol's reference price: its last fill.
    pub fn reference(&self, symbol: &str) -> Option<Decimal> {
        self.references.get(symbol).copied()
    }

    /// The price an order counts at: its limit, or for a Market order the reference now, or zero
    /// without one (allowed only where no notional or credit limit applies).
    pub fn risk_price(&self, symbol: &str, price: Option<Decimal>) -> Decimal {
        price.or_else(|| self.reference(symbol)).unwrap_or(Decimal::ZERO)
    }

    /// A fill of `qty` at `price` for the session `client`, added to its filled notional.
    /// `reference` is the venue's LastPx(31), if the report had a fill of its own: the symbol's
    /// new reference. A price derived from AvgPx for a fill the gateway missed is none. A
    /// LastPx of zero or less is no reference either (a venue leaving it at 0, say): the symbol
    /// keeps the one it has.
    pub fn fill(
        &mut self,
        client: &ClientId,
        symbol: &CompactString,
        qty: Decimal,
        price: Decimal,
        reference: Option<Decimal>,
    ) {
        match reference {
            Some(reference) if reference > Decimal::ZERO => {
                self.references.insert(symbol.clone(), reference);
            }
            Some(reference) => {
                tracing::warn!(%client, %symbol, %reference, "fill price not positive; the reference price stays");
            }
            None => {}
        }
        let session = self.sessions.entry(client.clone()).or_default();
        // Only ever added to, so saturating can't make the sum drift.
        session.filled = session.filled.checked_add(notional(qty, price)).unwrap_or(Decimal::MAX);
    }

    /// Records what order `id` counts for now, `None` once it's done, replacing what it counted
    /// for before.
    pub fn track(&mut self, id: OrderId, exposure: Option<Exposure>) {
        if self.exposures.get(&id) == exposure.as_ref() {
            return;
        }
        if let Some(old) = self.exposures.remove(&id) {
            let session = self.sessions.get_mut(&old.client).expect("a tracked order's session is kept");
            session.open -= old.notional;
            let key = (old.symbol, old.venue, buying(old.side));
            let resting = session.resting.get_mut(&key).expect("a tracked order rests");
            resting.retain(|(order, _)| *order != id);
            if resting.is_empty() {
                session.resting.remove(&key);
            }
        }
        if let Some(new) = exposure {
            let session = self.sessions.entry(new.client.clone()).or_default();
            session.open += new.notional;
            let key = (new.symbol.clone(), new.venue.clone(), buying(new.side));
            session.resting.entry(key).or_default().push((id, new.price));
            self.exposures.insert(id, new);
        }
    }

    /// What order `id` counts for, as last tracked.
    pub fn exposure(&self, id: OrderId) -> Option<&Exposure> {
        self.exposures.get(&id)
    }

    /// Whether `order` would trade with one of its session's live orders on the other side, same
    /// symbol and venue: a buy at or above a sell's price, a sell at or below a buy's. A Market
    /// order on either side crosses.
    fn crosses_own(&self, order: &Proposed) -> bool {
        let Some(session) = self.sessions.get(order.client) else { return false };
        let key = (CompactString::from(order.symbol), CompactString::from(order.venue), !buying(order.side));
        let Some(resting) = session.resting.get(&key) else { return false };
        resting.iter().any(|&(_, price)| match (order.price, price) {
            (Some(incoming), Some(resting)) if buying(order.side) => incoming >= resting,
            (Some(incoming), Some(resting)) => incoming <= resting,
            _ => true,
        })
    }

    /// Whether the session's open and filled notional, with `notional` for `order` in place of
    /// what a replaced order counts for now, stays within `credit`. A replace that doesn't raise
    /// the order's notional passes. Amounts too big to add up are over.
    fn within_credit(&self, order: &Proposed, notional: Decimal, credit: Decimal) -> bool {
        let current =
            order.replaces.and_then(|id| self.exposure(id)).map_or(Decimal::ZERO, |exposure| exposure.notional);
        if order.replaces.is_some() && notional <= current {
            return true;
        }
        let (open, filled) = self
            .sessions
            .get(order.client)
            .map_or((Decimal::ZERO, Decimal::ZERO), |session| (session.open, session.filled));
        let total = (open - current).checked_add(filled).and_then(|total| total.checked_add(notional));
        total.is_some_and(|total| total <= credit)
    }

    /// The state's invariants, given every live order's exposure as the book computes it now
    /// (`live`): what's tracked is exactly that, each session's open notional is their sum, and
    /// its resting orders are those orders. There are no more reference prices than the book has
    /// `orders`.
    #[cfg(debug_assertions)]
    pub fn check(&self, live: impl Iterator<Item = (OrderId, Exposure)>, orders: usize) {
        assert!(self.references.len() <= orders, "a reference price per fill on a booked order");
        let mut count = 0;
        for (id, exposure) in live {
            assert_eq!(self.exposures.get(&id), Some(&exposure), "order {id:?}'s exposure");
            count += 1;
        }
        assert_eq!(self.exposures.len(), count, "exposures of orders no longer live");
        for (client, session) in &self.sessions {
            let mine = || self.exposures.iter().filter(|(_, exposure)| exposure.client == *client);
            let open = mine().fold(Decimal::ZERO, |open, (_, exposure)| open + exposure.notional);
            assert_eq!(session.open, open, "{client}'s open notional");
            let resting: usize = session.resting.values().map(Vec::len).sum();
            assert_eq!(resting, mine().count(), "{client}'s resting orders");
            for (id, exposure) in mine() {
                let key = (exposure.symbol.clone(), exposure.venue.clone(), buying(exposure.side));
                assert!(session.resting[&key].contains(&(*id, exposure.price)), "{id:?} rests under its key");
            }
        }
    }
}

/// One CompID's limits. `None` (or `false`) is no check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    /// OrderQty(38) at most this.
    pub max_order_qty: Option<Decimal>,
    /// Quantity times price (a Market order's: the reference price) at most this.
    pub max_notional: Option<Decimal>,
    /// Only these symbols.
    pub symbols: Option<Vec<CompactString>>,
    /// A Limit order's price within this percentage either side of the symbol's reference price
    /// (its last fill); no check without one.
    pub price_band: Option<Decimal>,
    /// The session's open notional, plus what it has filled since start, plus the order's, at
    /// most this.
    pub credit: Option<Decimal>,
    /// Refuse an order that would trade with the same session's resting order on the same symbol
    /// and venue.
    pub self_trade: bool,
}

/// The limits for every CompID: its own, or the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub defaults: Limits,
    /// Each with the defaults' keys it doesn't set filled in.
    pub by_comp_id: HashMap<CompactString, Limits>,
}

impl Config {
    /// The limits the sessions of `comp_id` are checked against.
    pub fn limits(&self, comp_id: &str) -> &Limits {
        self.by_comp_id.get(comp_id).unwrap_or(&self.defaults)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(text: &str) -> Decimal {
        text.parse().unwrap()
    }

    #[test]
    fn max_order_notional_is_ten_to_the_eighteenth() {
        assert_eq!(MAX_ORDER_NOTIONAL, Decimal::from(10_u64.pow(18)));
    }

    /// Positive, capped, to four places, and an overflow is the cap rather than a panic.
    #[test]
    fn a_notional_is_capped_and_rounded() {
        assert_eq!(notional(decimal("3"), decimal("1.23456")), decimal("3.7037"));
        assert_eq!(notional(decimal("100"), decimal("-2")), decimal("200"));
        assert_eq!(notional(decimal("10000000000000000000"), decimal("1")), MAX_ORDER_NOTIONAL);
        assert_eq!(notional(Decimal::MAX, decimal("2")), MAX_ORDER_NOTIONAL);
    }

    #[test]
    fn the_band_includes_its_edges_and_refuses_what_overflows() {
        let (reference, band) = (decimal("10"), decimal("5"));
        assert!(within_band(decimal("10.5"), reference, band));
        assert!(within_band(decimal("9.5"), reference, band));
        assert!(!within_band(decimal("10.5001"), reference, band));
        assert!(!within_band(Decimal::MIN, Decimal::MAX, band));
    }

    /// Taking an order's exposure back out leaves the session as it was, however many came and
    /// went, as the book's invariant checks after every input.
    #[test]
    fn tracking_an_order_out_leaves_the_session_as_it_was() {
        let mut state = State::default();
        let client = ClientId::from("FUNDA");
        let exposure = |notional: &str, price: &str| Exposure {
            client: client.clone(),
            symbol: "AAPL".into(),
            venue: "NASDAQ".into(),
            side: Side::Buy,
            price: Some(decimal(price)),
            notional: decimal(notional),
        };
        state.track(OrderId(1), Some(exposure("0.0001", "1")));
        state.track(OrderId(2), Some(exposure("999999999999999999.9999", "2")));
        state.track(OrderId(2), Some(exposure("5", "2")));
        state.track(OrderId(2), None);
        state.check([(OrderId(1), exposure("0.0001", "1"))].into_iter(), 2);
        state.track(OrderId(1), None);
        state.check(std::iter::empty(), 2);
    }
}
