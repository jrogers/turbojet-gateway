//! Routing rules: ordered, first match wins. Kept to one function, [`Router::route`], so
//! failover or code-defined routing can replace it without touching the book.

use turbojet::fields::CompactString;

use crate::book::{ClientId, OrderRequest, VenueId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Venue(VenueId),
    /// Reject the order, with this Text(58).
    Reject(CompactString),
}

/// Matches an order when every condition it sets holds; an unset one matches anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub client: Option<ClientId>,
    pub ex_destination: Option<CompactString>,
    pub symbol: Option<CompactString>,
    pub target: Target,
}

impl Rule {
    fn matches(&self, client: &str, request: &OrderRequest) -> bool {
        self.client.as_deref().is_none_or(|c| c == client)
            && self.ex_destination.as_deref().is_none_or(|d| request.ex_destination.as_deref() == Some(d))
            && self.symbol.as_deref().is_none_or(|s| s == request.symbol.as_str())
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

    /// The first rule that matches decides; `None` when none does, which the book rejects as no
    /// route, telling it apart from a rule's own [`Target::Reject`].
    pub fn route(&self, client: &str, request: &OrderRequest) -> Option<Target> {
        self.rules.iter().find(|rule| rule.matches(client, request)).map(|rule| rule.target.clone())
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
