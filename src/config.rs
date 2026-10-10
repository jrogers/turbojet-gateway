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
//! ```
//!
//! The sessions file is Turbojet's (`turbojet-config`): its `[acceptor]` serves the clients, all
//! speaking its `begin_string`, and each `[initiator.NAME]` logs on to venue `NAME`. The
//! `sessions` key is read at start only: a reload rereads the routes, and the sessions file in
//! use. Turbojet-config's `validation` feature stays off: a dictionary on a venue initiator would
//! reject venue reports with custom codes before the gateway sees them.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::warn;
use turbojet_config::SessionsFile;

use crate::fix::Version;
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
    Ok(Loaded { router, sessions, version })
}

/// Reads the routes in the configuration file at `path` again, checking that each venue they
/// name is one of `venues`.
///
/// # Errors
///
/// As [`load`], for the routes, or a route names a venue not in `venues`.
pub fn reload_routes(path: &Path, venues: &[String]) -> Result<Router, Error> {
    let router = read(path)?.router(path)?;
    check_venues(path, &router, venues)?;
    Ok(router)
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
        let router = reload_routes(&path, &venues).unwrap();
        assert_eq!(router, Router::new(vec![venue_rule(Some("FUNDA"), "NASDAQ")]));
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"LSE\"").unwrap();
        assert!(reload_routes(&path, &venues).is_err());
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
