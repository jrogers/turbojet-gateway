//! Pre-trade risk: the limits each client's orders are checked against before they're routed.

use std::collections::HashMap;

use turbojet::fields::{CompactString, Decimal};

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
