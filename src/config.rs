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
    let sessions = SessionsFile::load(&sessions_path).map_err(|e| Error(e.to_string()))?;
    let Some(base) = sessions.base() else {
        return Err(Error(format!("{}: no [acceptor]: the clients log on to it", sessions_path.display())));
    };
    let version = base.begin_string.parse().map_err(|e| Error(format!("{}: {e}", sessions_path.display())))?;
    let router = file.router(&sessions).map_err(|e| Error(format!("{}: {e}", path.display())))?;
    Ok(Loaded { router, sessions, version })
}

/// Reads the routes in the configuration file at `path` again, checking them against `sessions`
/// as it is now.
///
/// # Errors
///
/// As [`load`], for the routes.
pub fn reload_routes(path: &Path, sessions: &SessionsFile) -> Result<Router, Error> {
    read(path)?.router(sessions).map_err(|e| Error(format!("{}: {e}", path.display())))
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
    /// The routes, each venue checked to be one of `sessions`' initiators.
    fn router(&self, sessions: &SessionsFile) -> Result<Router, String> {
        let venues = sessions.initiator_names();
        let mut rules = Vec::with_capacity(self.route.len());
        for (index, route) in self.route.iter().enumerate() {
            let number = index + 1;
            let target = match (&route.venue, &route.reject) {
                (Some(venue), None) if venues.contains(venue) => Target::Venue(venue.as_str().into()),
                (Some(venue), None) => {
                    return Err(format!("route {number}: venue {venue:?} has no [initiator.{venue}]"));
                }
                (None, Some(text)) => Target::Reject(text.as_str().into()),
                _ => return Err(format!("route {number}: needs one of venue and reject")),
            };
            rules.push(Rule {
                client: route.client.as_deref().map(Into::into),
                ex_destination: route.ex_destination.as_deref().map(Into::into),
                symbol: route.symbol.as_deref().map(Into::into),
                target,
            });
        }
        Ok(Router::new(rules))
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

    /// A directory of its own for each test, holding `files`.
    fn dir(test: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jetbridge-config-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn load_error(test: &str, jetbridge: &str) -> String {
        let dir = dir(test, &[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS)]);
        load(&dir.join("jetbridge.toml")).unwrap_err().to_string()
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
        let dir = dir("good", &[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS)]);
        let loaded = load(&dir.join("jetbridge.toml")).unwrap();
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
        let error =
            load_error("unknown-key", "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\nsymbl = \"X\"");
        assert!(error.contains("unknown field `symbl`"), "{error}");
    }

    #[test]
    fn a_route_needs_one_of_venue_and_reject() {
        let both = "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"\nreject = \"no\"";
        assert!(load_error("both", both).contains("route 1: needs one of venue and reject"));
        let neither = "sessions = \"sessions.toml\"\n[[route]]\nreject = \"no\"\n[[route]]\nsymbol = \"X\"";
        assert!(load_error("neither", neither).contains("route 2: needs one of venue and reject"));
    }

    #[test]
    fn a_route_to_a_venue_with_no_initiator_is_an_error() {
        let error = load_error("no-venue", "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"LSE\"");
        assert!(error.contains("venue \"LSE\" has no [initiator.LSE]"), "{error}");
    }

    #[test]
    fn the_acceptor_must_speak_a_version_the_gateway_converts() {
        let sessions = SESSIONS.replace("FIX.4.4", "FIX.4.3");
        let dir = dir("version", &[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", &sessions)]);
        let error = load(&dir.join("jetbridge.toml")).unwrap_err().to_string();
        assert!(error.contains("\"FIX.4.3\" is not supported"), "{error}");
    }

    #[test]
    fn the_sessions_file_needs_an_acceptor() {
        let sessions = "[initiator.NASDAQ]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"JETBRIDGE\"\n\
                        target_comp_id = \"NASDAQ\"\nconnect = [\"127.0.0.1:1\"]";
        let dir =
            dir("no-acceptor", &[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", sessions)]);
        let error = load(&dir.join("jetbridge.toml")).unwrap_err().to_string();
        assert!(error.contains("no [acceptor]"), "{error}");
    }

    /// The sessions path is relative to the configuration file, not the working directory.
    #[test]
    fn the_sessions_path_is_relative_to_the_file() {
        let jetbridge = "sessions = \"fix/sessions.toml\"\n[[route]]\nvenue = \"NASDAQ\"";
        let dir = dir("relative", &[("conf/jetbridge.toml", jetbridge), ("conf/fix/sessions.toml", SESSIONS)]);
        let loaded = load(&dir.join("conf/jetbridge.toml")).unwrap();
        assert_eq!(loaded.router, Router::new(vec![venue_rule(None, "NASDAQ")]));
    }

    #[test]
    fn reloading_replaces_the_routes() {
        let path = dir("reload", &[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", SESSIONS)])
            .join("jetbridge.toml");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.router, Router::new(Vec::new()));
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[[route]]\nclient = \"FUNDA\"\nvenue = \"NASDAQ\"")
            .unwrap();
        let router = reload_routes(&path, &loaded.sessions).unwrap();
        assert_eq!(router, Router::new(vec![venue_rule(Some("FUNDA"), "NASDAQ")]));
        std::fs::write(&path, "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"LSE\"").unwrap();
        assert!(reload_routes(&path, &loaded.sessions).is_err());
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
