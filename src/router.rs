//! Routing rules: ordered, first match wins. Kept to one function, [`Router::route_rule`] (which
//! [`Router::route`] wraps), so failover or code-defined routing can replace it without touching
//! the book, and the API's dry run follows it.

use serde::Serialize;
use turbojet::fields::CompactString;

use crate::book::{OrderRequest, VenueId};

/// Serialized as `{"venue": NAME}` or `{"reject": TEXT}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Venue(VenueId),
    /// Reject the order, with this Text(58).
    Reject(CompactString),
}

/// Matches an order when every condition it sets holds; an unset one matches anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The client's CompID: matches each of its sessions.
    pub client: Option<CompactString>,
    pub ex_destination: Option<CompactString>,
    pub symbol: Option<CompactString>,
    pub target: Target,
}

impl Rule {
    fn matches(&self, comp_id: &str, ex_destination: Option<&str>, symbol: &str) -> bool {
        self.client.as_deref().is_none_or(|c| c == comp_id)
            && self.ex_destination.as_deref().is_none_or(|d| ex_destination == Some(d))
            && self.symbol.as_deref().is_none_or(|s| s == symbol)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Router {
    rules: Vec<Rule>,
}

impl Router {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    /// The rules, in the order they're tried.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The first rule that matches decides; `None` when none does, which the book rejects as no
    /// route, telling it apart from a rule's own [`Target::Reject`]. `comp_id` is the client's CompID.
    pub fn route(&self, comp_id: &str, request: &OrderRequest) -> Option<Target> {
        self.route_rule(comp_id, request.ex_destination.as_deref(), &request.symbol)
            .map(|(_, rule)| rule.target.clone())
    }

    /// The rule [`route`](Self::route) follows, and its index in [`rules`](Self::rules): from only
    /// the fields the rules read, for a dry run.
    pub fn route_rule(&self, comp_id: &str, ex_destination: Option<&str>, symbol: &str) -> Option<(usize, &Rule)> {
        self.rules.iter().enumerate().find(|(_, rule)| rule.matches(comp_id, ex_destination, symbol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::{OrdType, Side};
    use turbojet::fields::Decimal;

    fn request(symbol: &str, ex_destination: Option<&str>) -> OrderRequest {
        OrderRequest {
            cl_ord_id: "1".into(),
            account: None,
            symbol: symbol.into(),
            side: Side::Buy,
            ord_type: OrdType::Limit,
            qty: Decimal::from(100),
            price: Some(Decimal::from(10)),
            ex_destination: ex_destination.map(Into::into),
            time_in_force: None,
        }
    }

    fn rule(client: Option<&str>, ex_destination: Option<&str>, symbol: Option<&str>, venue: &str) -> Rule {
        Rule {
            client: client.map(Into::into),
            ex_destination: ex_destination.map(Into::into),
            symbol: symbol.map(Into::into),
            target: Target::Venue(venue.into()),
        }
    }

    #[test]
    fn first_matching_rule_wins() {
        let router = Router::new(vec![
            rule(Some("FUNDA"), Some("XNAS"), None, "NASDAQ-A"),
            rule(None, Some("XNAS"), None, "NASDAQ-B"),
        ]);
        assert_eq!(router.route("FUNDA", &request("AAPL", Some("XNAS"))), Some(Target::Venue("NASDAQ-A".into())));
        assert_eq!(router.route("FUNDB", &request("AAPL", Some("XNAS"))), Some(Target::Venue("NASDAQ-B".into())));
        assert_eq!(router.route_rule("FUNDB", Some("XNAS"), "AAPL"), Some((1, &router.rules()[1])));
    }

    #[test]
    fn symbol_rules_match_on_symbol() {
        let router = Router::new(vec![rule(None, None, Some("VOD.L"), "LSE")]);
        assert_eq!(router.route("FUNDA", &request("VOD.L", None)), Some(Target::Venue("LSE".into())));
    }

    #[test]
    fn no_match_is_none() {
        let router = Router::new(vec![rule(None, Some("XNAS"), None, "NASDAQ")]);
        assert_eq!(router.route("FUNDA", &request("AAPL", None)), None);
    }

    #[test]
    fn a_reject_rule_rejects_with_its_text() {
        let router = Router::new(vec![Rule {
            client: Some("FUNDA".into()),
            ex_destination: None,
            symbol: None,
            target: Target::Reject("account closed".into()),
        }]);
        assert_eq!(router.route("FUNDA", &request("AAPL", None)), Some(Target::Reject("account closed".into())));
    }
}
