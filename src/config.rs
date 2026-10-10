//! The gateway's configuration file, `jetbridge.toml`: where its sessions file is, and its routes.
//!
//! ```toml
//! sessions = "sessions.toml"      # relative to this file
//!
//! [[route]]                       # first match wins
//! client = "FUNDA"                # SenderCompID; any, if left out
//! ex_destination = "XNAS"         # ExDestination(100); any, if left out
//! symbol = "AAPL"                 # Symbol(55); any, if left out
//! venue = "NASDAQ"                # an [initiator.NASDAQ] in the sessions file
//!
//! [[route]]
//! reject = "no route"             # or reject the order, with this Text(58)
//!
//! [risk.defaults]                 # every client's limits; each key optional, absent = no check
//! max_order_qty = 10000           # OrderQty(38) at most this
//! max_notional = 1000000          # quantity x price at most this
//! symbols = ["AAPL", "MSFT"]      # only these symbols
//! price_band = 5.0                # Limit price within 5% of the last fill on the symbol
//! credit = 5000000                # open + filled since start + this order's notional
//! self_trade = true               # refuse an order crossing the session's own resting one
//!
//! [risk.client.FUNDA]             # a CompID's own: each key it sets replaces the default's
//! max_notional = "250000.50"
//! ```
//!
//! Amounts are TOML integers, floats or strings, read as decimals: a string keeps every digit
//! exactly as written. Each must be positive. Limits are per CompID, but each of its sessions is
//! checked separately.
//!
//! The sessions file is Turbojet's (`turbojet-config`): its `[acceptor]` serves the clients, all
//! speaking its `begin_string`, and each `[initiator.NAME]` logs on to venue `NAME`. The
//! `sessions` key is read at start only: a reload rereads the routes and limits, and the sessions
//! file in use. Turbojet-config's `validation` feature stays off: a dictionary on a venue initiator would
//! reject venue reports with custom codes before the gateway sees them.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::warn;
use turbojet::fields::Decimal;
use turbojet_config::SessionsFile;

use crate::fix::Version;
use crate::risk::{self, Limits};
use crate::router::{Router, Rule, Target};

/// Why a configuration file can't be used: which file, and what's wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error {
    pub(crate) fn new(message: impl fmt::Display) -> Self {
        Self(message.to_string())
    }
}

impl std::error::Error for Error {}

/// A configuration, loaded and checked.
#[derive(Debug)]
pub struct Loaded {
    pub router: Router,
    pub limits: risk::Config,
    pub sessions: SessionsFile,
    /// The clients' version: the acceptor's BeginString.
    pub version: Version,
}

/// Reads the configuration file at `path` and the sessions file it names.
///
/// # Errors
///
/// Either file can't be read or doesn't load, the sessions file has no `[acceptor]` or one with a
/// BeginString the gateway doesn't convert, or a route names a venue with no initiator.
pub fn load(path: &Path) -> Result<Loaded, Error> {
    let file = read(path)?;
    let sessions_path = path.parent().unwrap_or(Path::new(".")).join(&file.sessions);
    let sessions = SessionsFile::load(&sessions_path).map_err(Error::new)?;
    let Some(base) = sessions.base() else {
        return Err(Error(format!("{}: no [acceptor]: the clients log on to it", sessions_path.display())));
    };
    let version = base.begin_string.parse().map_err(|e| Error(format!("{}: {e}", sessions_path.display())))?;
    let router = file.router(path)?;
    check_venues(path, &router, &sessions.initiator_names())?;
    let limits = file.risk.limits().map_err(|e| Error(format!("{}: {e}", path.display())))?;
    Ok(Loaded { router, limits, sessions, version })
}

/// Reads the routes and limits in the configuration file at `path` again, checking that each
/// venue the routes name is one of `venues`.
///
/// # Errors
///
/// As [`load`], for the routes and limits, or a route names a venue not in `venues`.
pub fn reload(path: &Path, venues: &[String]) -> Result<(Router, risk::Config), Error> {
    let file = read(path)?;
    let router = file.router(path)?;
    check_venues(path, &router, venues)?;
    let limits = file.risk.limits().map_err(|e| Error(format!("{}: {e}", path.display())))?;
    Ok((router, limits))
}

/// Checks that each venue `router`, read from the file at `path`, routes to is one of `venues`:
/// the initiators, or those the gateway took (see [`Gateway::set_venues`](crate::edge::Gateway::set_venues)).
///
/// # Errors
///
/// The first route to a venue not in `venues`.
pub fn check_venues(path: &Path, router: &Router, venues: &[String]) -> Result<(), Error> {
    for (index, rule) in router.rules().iter().enumerate() {
        if let Target::Venue(venue) = &rule.target
            && !venues.iter().any(|v| v == venue.as_str())
        {
            return Err(Error(format!(
                "{}: route {}: venue {venue:?} has no [initiator.{venue}], or one whose BeginString \
                 the gateway doesn't convert",
                path.display(),
                index + 1
            )));
        }
    }
    Ok(())
}

/// What's likely a mistake in `router`, though it loads: routes after one that matches every
/// order, which can never match, and no routes at all, which rejects every order.
pub fn warnings(router: &Router) -> Vec<String> {
    let rules = router.rules();
    if rules.is_empty() {
        return vec!["no routes: every order is rejected".to_string()];
    }
    let catch_all = rules.iter().position(|r| r.client.is_none() && r.ex_destination.is_none() && r.symbol.is_none());
    let Some(catch_all) = catch_all else { return Vec::new() };
    (catch_all + 1..rules.len())
        .map(|index| format!("route {} can never match: route {} matches every order", index + 1, catch_all + 1))
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    sessions: PathBuf,
    #[serde(default)]
    route: Vec<Route>,
    #[serde(default)]
    risk: Risk,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Risk {
    #[serde(default)]
    defaults: RiskTable,
    /// By CompID; ordered so the first bad one reported is the same each time.
    #[serde(default)]
    client: BTreeMap<String, RiskTable>,
}

/// `[risk.defaults]` or `[risk.client.COMPID]`, as written.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskTable {
    max_order_qty: Option<Amount>,
    max_notional: Option<Amount>,
    symbols: Option<Vec<String>>,
    price_band: Option<Amount>,
    credit: Option<Amount>,
    self_trade: Option<bool>,
}

/// A decimal as TOML writes it: a float goes through its shortest exact form, so `0.1` is 0.1.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Amount {
    Integer(i64),
    Float(f64),
    Text(String),
}

impl Risk {
    /// The limits per CompID, each client table's keys over the defaults'.
    fn limits(&self) -> Result<risk::Config, Error> {
        let defaults = self.defaults.limits("risk.defaults", &Limits::default())?;
        let mut by_comp_id = HashMap::with_capacity(self.client.len());
        for (comp_id, table) in &self.client {
            let limits = table.limits(&format!("risk.client.{comp_id}"), &defaults)?;
            by_comp_id.insert(comp_id.as_str().into(), limits);
        }
        Ok(risk::Config { defaults, by_comp_id })
    }
}

impl RiskTable {
    /// The table's limits, `under` naming it in errors, with `defaults`' for the keys it leaves out.
    fn limits(&self, under: &str, defaults: &Limits) -> Result<Limits, Error> {
        let amount = |key: &str, amount: &Option<Amount>, default: Option<Decimal>| match amount {
            None => Ok(default),
            Some(amount) => amount.positive().map(Some).map_err(|e| Error(format!("{under}.{key}: {e}"))),
        };
        let symbols = match &self.symbols {
            None => defaults.symbols.clone(),
            Some(symbols) if symbols.is_empty() => {
                return Err(Error(format!(
                    "{under}.symbols: empty, which refuses every order; leave it out to allow any"
                )));
            }
            Some(symbols) => Some(symbols.iter().map(|symbol| symbol.as_str().into()).collect()),
        };
        Ok(Limits {
            max_order_qty: amount("max_order_qty", &self.max_order_qty, defaults.max_order_qty)?,
            max_notional: amount("max_notional", &self.max_notional, defaults.max_notional)?,
            symbols,
            price_band: amount("price_band", &self.price_band, defaults.price_band)?,
            credit: amount("credit", &self.credit, defaults.credit)?,
            self_trade: self.self_trade.unwrap_or(defaults.self_trade),
        })
    }
}

impl Amount {
    fn positive(&self) -> Result<Decimal, String> {
        let text = match self {
            Self::Integer(integer) => integer.to_string(),
            Self::Float(float) => float.to_string(),
            Self::Text(text) => text.clone(),
        };
        let amount: Decimal = text.parse().map_err(|_| format!("{text:?} is not a decimal"))?;
        if amount <= Decimal::ZERO {
            return Err(format!("{text} is not positive"));
        }
        Ok(amount)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    client: Option<String>,
    ex_destination: Option<String>,
    symbol: Option<String>,
    venue: Option<String>,
    reject: Option<String>,
}

fn read(path: &Path) -> Result<File, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error(format!("{}: {e}", path.display())))?;
    toml::from_str(&text).map_err(|e| Error(format!("{}: {e}", path.display())))
}

impl File {
    /// The routes, read from `path`, with their [`warnings`] logged.
    fn router(&self, path: &Path) -> Result<Router, Error> {
        let mut rules = Vec::with_capacity(self.route.len());
        for (index, route) in self.route.iter().enumerate() {
            let target = match (&route.venue, &route.reject) {
                (Some(venue), None) => Target::Venue(venue.as_str().into()),
                (None, Some(text)) => Target::Reject(text.as_str().into()),
                _ => {
                    let number = index + 1;
                    return Err(Error(format!("{}: route {number}: needs one of venue and reject", path.display())));
                }
            };
            rules.push(Rule {
                client: route.client.as_deref().map(Into::into),
                ex_destination: route.ex_destination.as_deref().map(Into::into),
                symbol: route.symbol.as_deref().map(Into::into),
                target,
            });
        }
        let router = Router::new(rules);
        for warning in warnings(&router) {
            warn!(file = %path.display(), "{warning}");
        }
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSIONS: &str = r#"
[acceptor]
begin_string = "FIX.4.4"
sender_comp_id = "JETBRIDGE"
listen = "127.0.0.1:0"
unknown = "admit"

[initiator.NASDAQ]
target_comp_id = "NASDAQ"
connect = ["127.0.0.1:1"]
"#;

    /// A temporary directory holding `files`.
    fn dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn load_error(jetbridge: &str) -> String {
        let dir = dir(&[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS)]);
        load(&dir.path().join("jetbridge.toml")).unwrap_err().to_string()
    }

    fn venue_rule(client: Option<&str>, venue: &str) -> Rule {
        Rule { client: client.map(Into::into), ex_destination: None, symbol: None, target: Target::Venue(venue.into()) }
    }

    #[test]
    fn loads_routes_in_order_and_the_clients_version() {
        let jetbridge = r#"
sessions = "sessions.toml"
[[route]]
client = "FUNDA"
ex_destination = "XNAS"
symbol = "AAPL"
venue = "NASDAQ"
[[route]]
reject = "no route"
"#;
        let dir = dir(&[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS)]);
        let loaded = load(&dir.path().join("jetbridge.toml")).unwrap();
        let expected = Router::new(vec![
            Rule {
                client: Some("FUNDA".into()),
                ex_destination: Some("XNAS".into()),
                symbol: Some("AAPL".into()),
                target: Target::Venue("NASDAQ".into()),
            },
            Rule { client: None, ex_destination: None, symbol: None, target: Target::Reject("no route".into()) },
        ]);
        assert_eq!(loaded.router, expected);
        assert_eq!(loaded.version, Version::Fix44);
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        let error = load_error("sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\nsymbl = \"X\"");
        assert!(error.contains("unknown field `symbl`"), "{error}");
    }

    #[test]
    fn a_route_needs_one_of_venue_and_reject() {
        let both = "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\nreject = \"no\"";
        assert!(load_error(both).contains("route 1: needs one of venue and reject"));
        let neither = "sessions = \"sessions.toml\"\n[[route]]\nreject = \"no\"\n[[route]]\nsymbol = \"X\"";
        assert!(load_error(neither).contains("route 2: needs one of venue and reject"));
    }

    #[test]
    fn a_route_to_a_venue_with_no_initiator_is_an_error() {
        let error = load_error("sessions = \"sessions.toml\"\n[[route]]\nvenue = \"LSE\"");
        assert!(error.contains("venue \"LSE\" has no [initiator.LSE]"), "{error}");
    }

    #[test]
    fn the_acceptor_must_speak_a_version_the_gateway_converts() {
        let sessions = SESSIONS.replace("FIX.4.4", "FIX.4.3");
        let dir = dir(&[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", &sessions)]);
        let error = load(&dir.path().join("jetbridge.toml")).unwrap_err().to_string();
        assert!(error.contains("\"FIX.4.3\" is not supported"), "{error}");
    }

    #[test]
    fn the_sessions_file_needs_an_acceptor() {
        let sessions = "[initiator.NASDAQ]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"JETBRIDGE\"\n\
                        target_comp_id = \"NASDAQ\"\nconnect = [\"127.0.0.1:1\"]";
        let dir = dir(&[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", sessions)]);
        let error = load(&dir.path().join("jetbridge.toml")).unwrap_err().to_string();
        assert!(error.contains("no [acceptor]"), "{error}");
    }

    /// The sessions path is relative to the configuration file, not the working directory.
    #[test]
    fn the_sessions_path_is_relative_to_the_file() {
        let jetbridge = "sessions = \"fix/sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"";
        let dir = dir(&[("conf/jetbridge.toml", jetbridge), ("conf/fix/sessions.toml", SESSIONS)]);
        let loaded = load(&dir.path().join("conf/jetbridge.toml")).unwrap();
        assert_eq!(loaded.router, Router::new(vec![venue_rule(None, "NASDAQ")]));
    }

    #[test]
    fn reloading_replaces_the_routes() {
        let dir = dir(&[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", SESSIONS)]);
        let path = dir.path().join("jetbridge.toml");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.router, Router::new(Vec::new()));
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[[route]]\nclient = \"FUNDA\"\nvenue = \"NASDAQ\"")
            .unwrap();
        let venues = loaded.sessions.initiator_names();
        let (router, _) = reload(&path, &venues).unwrap();
        assert_eq!(router, Router::new(vec![venue_rule(Some("FUNDA"), "NASDAQ")]));
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"LSE\"").unwrap();
        assert!(reload(&path, &venues).is_err());
    }

    fn limits(risk: &str) -> Result<risk::Config, String> {
        let dir =
            dir(&[("jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{risk}")), ("sessions.toml", SESSIONS)]);
        load(&dir.path().join("jetbridge.toml")).map(|loaded| loaded.limits).map_err(|e| e.to_string())
    }

    fn decimal(text: &str) -> Decimal {
        text.parse().unwrap()
    }

    /// A CompID's keys replace the defaults' one by one; the keys it leaves out are the defaults'.
    /// Integers, floats and strings are all decimals.
    #[test]
    fn a_comp_ids_limits_are_its_keys_over_the_defaults() {
        let risk = r#"
[risk.defaults]
max_order_qty = 1000
max_notional = 50000.5
symbols = ["AAPL"]
self_trade = true
[risk.client.FUNDA]
max_order_qty = "250.25"
price_band = 5.0
credit = 1000000
self_trade = false
"#;
        let config = limits(risk).unwrap();
        let defaults = Limits {
            max_order_qty: Some(decimal("1000")),
            max_notional: Some(decimal("50000.5")),
            symbols: Some(vec!["AAPL".into()]),
            price_band: None,
            credit: None,
            self_trade: true,
        };
        assert_eq!(config.limits("FUNDB"), &defaults);
        let funda = Limits {
            max_order_qty: Some(decimal("250.25")),
            price_band: Some(decimal("5")),
            credit: Some(decimal("1000000")),
            self_trade: false,
            ..defaults.clone()
        };
        assert_eq!(config.limits("FUNDA"), &funda);
    }

    #[test]
    fn no_risk_tables_is_no_limits() {
        assert_eq!(limits("").unwrap(), risk::Config::default());
        assert_eq!(limits("").unwrap().limits("FUNDA"), &Limits::default());
    }

    #[test]
    fn a_bad_limit_is_an_error_naming_its_key() {
        let cases = [
            ("[risk.defaults]\nmax_order_qty = 0", "risk.defaults.max_order_qty: 0 is not positive"),
            ("[risk.client.FUNDA]\ncredit = -5", "risk.client.FUNDA.credit: -5 is not positive"),
            ("[risk.defaults]\nmax_notional = \"lots\"", "risk.defaults.max_notional: \"lots\" is not a decimal"),
            ("[risk.defaults]\nprice_band = nan", "risk.defaults.price_band: \"NaN\" is not a decimal"),
            ("[risk.defaults]\nsymbols = []", "risk.defaults.symbols: empty"),
            ("[risk.defaults]\nmax_qty = 5", "unknown field `max_qty`"),
            ("[risk.client.FUNDA]\nself_trade = \"yes\"", "invalid type"),
            ("[risk.limits]\ncredit = 5", "unknown field `limits`"),
        ];
        for (risk, expected) in cases {
            let error = limits(risk).unwrap_err();
            assert!(error.contains(expected), "{risk}: {error}");
        }
    }

    /// A reload returns the new limits with the routes; one that fails returns neither.
    #[test]
    fn reloading_replaces_the_limits_with_the_routes() {
        let dir = dir(&[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", SESSIONS)]);
        let path = dir.path().join("jetbridge.toml");
        let venues = load(&path).unwrap().sessions.initiator_names();
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[risk.defaults]\nmax_order_qty = 5").unwrap();
        let (_, limits) = reload(&path, &venues).unwrap();
        assert_eq!(limits.limits("FUNDA").max_order_qty, Some(decimal("5")));
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[risk.defaults]\nmax_order_qty = -5").unwrap();
        assert!(reload(&path, &venues).is_err());
    }

    #[test]
    fn routes_that_reject_every_order_or_never_match_warn() {
        assert_eq!(warnings(&Router::new(Vec::new())), ["no routes: every order is rejected"]);
        let catch_all = Rule { client: None, ex_destination: None, symbol: None, target: Target::Reject("no".into()) };
        let router = Router::new(vec![venue_rule(Some("FUNDA"), "NASDAQ"), catch_all, venue_rule(Some("FUNDB"), "X")]);
        assert_eq!(warnings(&router), ["route 3 can never match: route 2 matches every order"]);
        assert!(warnings(&Router::new(vec![venue_rule(Some("FUNDA"), "NASDAQ")])).is_empty());
    }

    /// The example files the readme shows load.
    #[test]
    fn the_examples_load() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/jetbridge.toml");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.version, Version::Fix44);
        assert_eq!(loaded.sessions.initiator_names(), ["NASDAQ"]);
    }
}
