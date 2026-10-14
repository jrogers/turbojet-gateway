//! The gateway's configuration file, `jetbridge.toml`: where its sessions file is, and its routes.
//!
//! ```toml
//! sessions = "sessions.toml"      # relative to this file
//! day_end = "17:00 America/New_York"  # when client sessions' trading days end
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
//! credit = 5000000                # open + filled today + this order's notional
//! self_trade = true               # refuse an order crossing the session's own resting one
//!
//! [risk.client.FUNDA]             # a CompID's own: each key it sets replaces the default's
//! max_notional = "250000.50"
//!
//! [database]                      # optional: without it, the book is kept in memory only
//! url = "sqlite://jetbridge.db?mode=rwc"  # `mode=rwc` creates the file
//! retention = "400d"              # optional: delete done orders older; a number and s, m, h or d
//!
//! [api]                           # optional: the operator API, an HTTP JSON server
//! listen = "127.0.0.1:8080"       # an IP address and port
//! users = "users.toml"            # who may use it; relative to this file
//! audit_dir = "audit"             # what they did, a file a day; relative to this file
//! audit_retention = "400d"        # optional: delete days older; as [message_log] retention
//! tls = { cert = "api.pem", key = "api.key" }  # optional, PEM; needs the `tls` feature
//! behind_proxy = false            # true: a proxy in front terminates TLS
//!
//! [message_log]                   # optional: every message, both sides', in files
//! dir = "log"                     # relative to this file
//! file_bytes_max = 268435456      # optional: a new file past this size; 256 MiB if left out
//! retention = "90d"               # optional: delete files older; a number and s, m, h or d
//! compress = true                 # optional: gzip finished files; needs the `gzip` feature
//! ```
//!
//! Amounts are TOML integers, floats or strings, read as decimals: a string keeps every digit
//! exactly as written. Each must be positive. Limits are per CompID, but each of its sessions is
//! checked separately.
//!
//! The sessions file is Turbojet's (`turbojet-config`): its `[acceptor]` serves the clients, all
//! speaking its `begin_string`, and each `[initiator.NAME]` logs on to venue `NAME`. The
//! `sessions` key is read at start only: a reload rereads the routes and limits, and the sessions
//! file in use. A reload keeps the `[database] url` and the `[message_log]` in use: a changed one
//! is a warning, for the next start. It takes a changed `[database] retention`. The log masks
//! passwords.
//!
//! With `[database] retention`, the gateway deletes, hourly, the done orders of trading days that
//! ended longer ago than that, with their rows; without it, nothing is deleted. It must be at
//! least a day.
//!
//! A client session's trading day ends at `day_end`, or sooner when its schedule's period (from
//! the sessions file) ends. `day_end` is a time, `HH:MM` or `HH:MM:SS`, and a time zone, `UTC`,
//! an offset (`+05:30`) or an IANA name, UTC if left out; midnight UTC if left out.
//!
//! Turbojet-config's `validation` feature stays off: a dictionary on a venue initiator would
//! reject venue reports with custom codes before the gateway sees them.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveTime;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::warn;
use turbojet::fields::Decimal;
use turbojet::schedule::ScheduleTimeZone;
use turbojet::{FileLogOptions, FileMessageLog, SessionSchedule, SessionStorage};
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
    /// The limits as the file writes them.
    pub risk: RiskTables,
    pub sessions: SessionsFile,
    /// The clients' version: the acceptor's BeginString.
    pub version: Version,
    /// When a session with no schedule ends its trading day: see [`day_end`].
    pub day_end: SessionSchedule,
    /// The `[database]` URL the book is kept in, if any: read at start only.
    pub database: Option<String>,
    /// `[database] retention`: how long after its trading day ends a done order is kept; for
    /// ever if left out. At least [`DATABASE_RETENTION_MIN`].
    pub retention: Option<Duration>,
    /// The operator API, if the file has `[api]`.
    pub api: Option<Api>,
    /// The message log, if the file has `[message_log]`: open, and given to the sessions file.
    pub message_log: Option<MessageLog>,
}

/// `[api]`: the operator API's server, its paths resolved against the configuration file's
/// directory and the bind rule checked (see the module documentation).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    pub listen: SocketAddr,
    /// The users file.
    pub users: PathBuf,
    /// The audit log's directory.
    pub audit_dir: PathBuf,
    /// How long a day's audit file is kept after its day ends; for ever if left out.
    #[serde(default, deserialize_with = "retention")]
    pub audit_retention: Option<Duration>,
    pub tls: Option<ApiTls>,
    /// A proxy in front terminates TLS, so a non-loopback `listen` without `tls` is fine.
    #[serde(default)]
    pub behind_proxy: bool,
}

/// `[api] tls`: the server's certificate chain and private key, PEM files.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiTls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// `[message_log]`: where the gateway logs every message its sessions receive and send, its `dir`
/// resolved against the configuration file's directory.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageLog {
    pub dir: PathBuf,
    /// Turbojet's default if left out.
    pub file_bytes_max: Option<NonZeroU64>,
    /// Files are kept for ever if left out.
    #[serde(default, deserialize_with = "retention")]
    pub retention: Option<Duration>,
    #[serde(default)]
    pub compress: bool,
}

impl MessageLog {
    /// Opens the log, creating its directory if need be. Passwords are masked: Turbojet's default.
    fn open(&self) -> std::io::Result<FileMessageLog> {
        let mut options = FileLogOptions::default();
        if let Some(bytes) = self.file_bytes_max {
            options.file_bytes_max = bytes.get();
        }
        options.retention = self.retention;
        options.compress = self.compress;
        FileMessageLog::open(&self.dir, options)
    }
}

/// Reads the configuration file at `path` and the sessions file it names, registering `stores`
/// with it by name (the crash tests' gated stores, say), and opens the `[message_log]`, if any,
/// for the sessions file's sessions.
///
/// # Errors
///
/// Either file can't be read or doesn't load, the sessions file has no `[acceptor]` or one with a
/// BeginString the gateway doesn't convert, a route names a venue with no initiator, or the
/// message log can't be opened.
pub(crate) fn load(path: &Path, stores: Vec<(String, Arc<dyn SessionStorage>)>) -> Result<Loaded, Error> {
    let file = read(path)?;
    let router = file.router(path, true)?;
    let limits = file.risk.limits().map_err(|e| Error(format!("{}: {e}", path.display())))?;
    let day_end = file.day_end(path)?;
    let api = file.api(path)?;
    let message_log = file.message_log(path)?;
    let sessions_path = path.parent().unwrap_or(Path::new(".")).join(&file.sessions);
    let mut builder = stores
        .into_iter()
        .fold(SessionsFile::builder(&sessions_path), |builder, (name, storage)| builder.with_store(name, storage));
    // ponytail: the builder takes the log before it loads the sessions file, so a sessions file
    // that doesn't load leaves the log's directory and an empty file; load it twice if that matters.
    if let Some(log) = &message_log {
        let opened = log
            .open()
            .map_err(|e| Error(format!("{}: [message_log] cannot open {}: {e}", path.display(), log.dir.display())))?;
        builder = builder.with_message_log(Arc::new(opened));
    }
    let sessions = builder.load().map_err(Error::new)?;
    let Some(base) = sessions.base() else {
        return Err(Error(format!("{}: no [acceptor]: the clients log on to it", sessions_path.display())));
    };
    let version = base.begin_string.parse().map_err(|e| Error(format!("{}: {e}", sessions_path.display())))?;
    check_venues(path, &router, &sessions.initiator_names())?;
    let (database, retention) = file.database();
    Ok(Loaded { router, limits, risk: file.risk, sessions, version, day_end, database, retention, api, message_log })
}

/// What a reload rereads from the configuration file.
#[derive(Debug)]
pub struct Reread {
    pub router: Router,
    pub limits: risk::Config,
    /// The limits as the file writes them.
    pub risk: RiskTables,
    pub day_end: SessionSchedule,
    pub api: Option<Api>,
    /// Not opened: a reload keeps the log in use.
    pub message_log: Option<MessageLog>,
    /// Not opened: a reload keeps the database in use.
    pub database: Option<String>,
    /// Taken: see [`Loaded::retention`].
    pub retention: Option<Duration>,
}

/// Reads the routes, limits, `day_end`, `[api]`, `[message_log]` and `[database]` in the
/// configuration file at `path` again, checking that each venue the routes name is one of
/// `venues`.
///
/// # Errors
///
/// As at start, for the routes, limits, `day_end`, `[api]` and `[message_log]`, or a route names
/// a venue not in `venues`. An `[api]` that doesn't load (or breaks the bind rule), or a
/// `[message_log]` that doesn't, refuses the whole file: the routes, limits and `day_end` in it
/// aren't taken either.
pub fn reload(path: &Path, venues: &[String]) -> Result<Reread, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error(format!("{}: {e}", path.display())))?;
    reread(path, &text, venues, true)
}

/// As [`reload`], from `text` rather than the file at `path`, which only names it in errors and
/// is what its relative paths are relative to: how a limit edit checks the text before writing it.
/// Logs the warnings `reload` does only if `log`.
///
/// # Errors
///
/// As [`reload`].
pub fn reread(path: &Path, text: &str, venues: &[String], log: bool) -> Result<Reread, Error> {
    let file = parse(path, text)?;
    let router = file.router(path, log)?;
    check_venues(path, &router, venues)?;
    let limits = file.risk.limits().map_err(|e| Error(format!("{}: {e}", path.display())))?;
    let day_end = file.day_end(path)?;
    let api = file.api(path)?;
    let message_log = file.message_log(path)?;
    let (database, retention) = file.database();
    Ok(Reread { router, limits, risk: file.risk, day_end, api, message_log, database, retention })
}

/// `day_end` as written, `HH:MM[:SS] [ZONE]`, as a schedule whose periods run from one end of day
/// to the next: the period containing a time ends when its trading day does, at the latest.
/// Turbojet's schedules read the time and zone, and handle daylight saving.
///
/// # Errors
///
/// The time or zone doesn't parse, or there's more.
pub fn day_end(text: &str) -> Result<SessionSchedule, String> {
    let mut words = text.split_whitespace();
    let time = words.next().ok_or("empty; expected a time and a time zone, as \"17:00 America/New_York\"")?;
    let time = NaiveTime::parse_from_str(time, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(time, "%H:%M"))
        .map_err(|_| format!("invalid time {time:?} (expected HH:MM or HH:MM:SS)"))?;
    let zone: ScheduleTimeZone = words.next().unwrap_or("UTC").parse()?;
    if let Some(extra) = words.next() {
        return Err(format!("unexpected {extra:?} after the time zone"));
    }
    Ok(SessionSchedule::daily(time, time).in_time_zone(zone))
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
    day_end: Option<String>,
    #[serde(default)]
    route: Vec<Route>,
    #[serde(default)]
    risk: RiskTables,
    database: Option<Database>,
    api: Option<Api>,
    message_log: Option<MessageLog>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Database {
    url: String,
    #[serde(default, deserialize_with = "database_retention")]
    retention: Option<Duration>,
}

/// The shortest `[database] retention`: a pruned order's requests are then over a day older than
/// any orphan a start finds, which is what lets the orphan scan take requests sent no later than
/// [`Store::pruned_through`](crate::store::Store::pruned_through) for a pruned order's.
pub const DATABASE_RETENTION_MIN: Duration = Duration::from_secs(86_400);

/// `[risk]` as written: the API shows it so (`GET /api/risk/limits`), the client tables as
/// `clients`.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RiskTables {
    #[serde(default)]
    pub defaults: RiskTable,
    /// By CompID; ordered so the first bad one reported is the same each time.
    #[serde(default, rename(serialize = "clients"))]
    pub client: BTreeMap<String, RiskTable>,
}

/// `[risk.defaults]` or `[risk.client.COMPID]`, as written: the keys left out are absent.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RiskTable {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_order_qty: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_notional: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    price_band: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credit: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    self_trade: Option<bool>,
}

/// A decimal as TOML writes it: a float goes through its shortest exact form, so `0.1` is 0.1.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum Amount {
    Integer(i64),
    Float(f64),
    Text(String),
}

impl RiskTables {
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

/// Checks the risk table `under` names in `text` (`risk.defaults` or `risk.client.COMPID`) on its
/// own: its keys and values, which don't depend on the rest of the file. A table that isn't there,
/// or text that doesn't parse, passes: loading the whole file says why.
///
/// # Errors
///
/// The table has an unknown key, or a value the loader refuses: why, naming the key.
pub fn check_risk_table(text: &str, under: &str) -> Result<(), String> {
    let Ok(file) = toml::from_str::<toml::Table>(text) else { return Ok(()) };
    let mut item = Some(&file);
    for key in under.split('.') {
        item = item.and_then(|table| table.get(key)).and_then(toml::Value::as_table);
    }
    let Some(table) = item else { return Ok(()) };
    let table: RiskTable = toml::Value::Table(table.clone()).try_into().map_err(|e| format!("{under}: {e}"))?;
    table.limits(under, &Limits::default()).map(drop).map_err(|e| e.to_string())
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

/// The longest `[message_log] retention`, `[api] audit_retention` or `[database] retention`:
/// longer than any record-keeping rule asks for, and short enough that the logs' and the prune's
/// date arithmetic can't overflow, so a typo is refused at load and reload rather than at the next
/// start's open.
const RETENTION_MAX: Duration = Duration::from_secs(100 * 366 * 86_400);

/// `[message_log] retention`, `[api] audit_retention` and `[database] retention`: a whole number
/// and a unit, `s`, `m`, `h` or `d` (`90d`), as the sessions file writes its durations, with days
/// added; positive, and at most [`RETENTION_MAX`].
fn retention<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Duration>, D::Error> {
    let text = String::deserialize(deserializer)?;
    let invalid = || serde::de::Error::custom(format!("invalid duration {text:?} (expected e.g. 12h or 90d)"));
    let (count, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let count: u64 = count.parse().map_err(|_| invalid())?;
    let unit_secs: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    match count.checked_mul(unit_secs).map(Duration::from_secs) {
        Some(Duration::ZERO) => {
            Err(serde::de::Error::custom("retention must be positive; leave it out to keep every file"))
        }
        Some(retention) if retention <= RETENTION_MAX => Ok(Some(retention)),
        _ => Err(serde::de::Error::custom(format!("retention {text:?} is longer than 100 years"))),
    }
}

/// `[database] retention`: as [`retention`], and at least [`DATABASE_RETENTION_MIN`].
fn database_retention<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Duration>, D::Error> {
    let text = String::deserialize(deserializer)?;
    let retention = retention(serde::de::value::StrDeserializer::<D::Error>::new(&text))?;
    if retention.is_some_and(|retention| retention < DATABASE_RETENTION_MIN) {
        return Err(serde::de::Error::custom(format!("[database] retention {text:?}: at least 1d")));
    }
    Ok(retention)
}

fn read(path: &Path) -> Result<File, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error(format!("{}: {e}", path.display())))?;
    parse(path, &text)
}

/// The file at `path`, whose text is `text`.
fn parse(path: &Path, text: &str) -> Result<File, Error> {
    toml::from_str(text).map_err(|e| Error(format!("{}: {e}", path.display())))
}

impl File {
    /// [`day_end`], midnight UTC if the file leaves it out.
    fn day_end(&self, path: &Path) -> Result<SessionSchedule, Error> {
        day_end(self.day_end.as_deref().unwrap_or("00:00 UTC"))
            .map_err(|e| Error(format!("{}: day_end: {e}", path.display())))
    }

    /// `[database]`'s URL and retention.
    fn database(&self) -> (Option<String>, Option<Duration>) {
        self.database.as_ref().map_or((None, None), |database| (Some(database.url.clone()), database.retention))
    }

    /// `[api]`, its paths made relative to `path`'s directory, if the build and the bind rule
    /// allow it.
    fn api(&self, path: &Path) -> Result<Option<Api>, Error> {
        let Some(api) = &self.api else { return Ok(None) };
        let error = |message: String| Error(format!("{}: [api] {message}", path.display()));
        if api.tls.is_some() && !cfg!(feature = "tls") {
            return Err(error("tls: this build has no TLS; build with the `tls` feature".to_string()));
        }
        if !api.listen.ip().to_canonical().is_loopback() && api.tls.is_none() && !api.behind_proxy {
            return Err(error(format!(
                "listen = \"{}\" isn't a loopback address, and without tls the API's passwords and \
                 tokens would cross the network in the clear: set tls, or behind_proxy = true if a \
                 proxy in front terminates TLS",
                api.listen
            )));
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        let tls = api.tls.as_ref().map(|tls| ApiTls { cert: dir.join(&tls.cert), key: dir.join(&tls.key) });
        Ok(Some(Api { users: dir.join(&api.users), audit_dir: dir.join(&api.audit_dir), tls, ..api.clone() }))
    }

    /// `[message_log]`, its `dir` made relative to `path`'s directory, if the build allows it.
    fn message_log(&self, path: &Path) -> Result<Option<MessageLog>, Error> {
        let Some(log) = &self.message_log else { return Ok(None) };
        if log.compress && !cfg!(feature = "gzip") {
            return Err(Error(format!(
                "{}: [message_log] compress: this build can't compress; build with the `gzip` feature",
                path.display()
            )));
        }
        let dir = path.parent().unwrap_or(Path::new(".")).join(&log.dir);
        Ok(Some(MessageLog { dir, ..log.clone() }))
    }

    /// The routes, read from `path`, with their [`warnings`] logged if `log`.
    fn router(&self, path: &Path, log: bool) -> Result<Router, Error> {
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
        if log {
            for warning in warnings(&router) {
                warn!(file = %path.display(), "{warning}");
            }
        }
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(path: &Path) -> Result<Loaded, Error> {
        super::load(path, Vec::new())
    }

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
    fn the_database_url_is_optional_and_its_table_takes_no_other_key() {
        let sessions = format!("{SESSIONS}store = \"disk\"\n[store.disk]\nkind = \"disk\"\ndir = \"store\"\n");
        let load_database = |jetbridge: &str| {
            let dir = dir(&[("jetbridge.toml", jetbridge), ("sessions.toml", &sessions)]);
            load(&dir.path().join("jetbridge.toml")).unwrap().database
        };
        assert_eq!(load_database("sessions = \"sessions.toml\""), None);
        let with = "sessions = \"sessions.toml\"\n[database]\nurl = \"sqlite://j.db?mode=rwc\"";
        assert_eq!(load_database(with).as_deref(), Some("sqlite://j.db?mode=rwc"));
        let error = load_error("sessions = \"sessions.toml\"\n[database]\nurl = \"sqlite://j.db\"\npath = \"x\"");
        assert!(error.contains("unknown field `path`"), "{error}");
    }

    /// `[database] retention` is optional, read as `[message_log]`'s, and at least a day, at load
    /// and reload.
    #[test]
    fn the_database_retention_is_optional_and_at_least_a_day() {
        let jetbridge =
            |more: &str| format!("sessions = \"sessions.toml\"\n[database]\nurl = \"sqlite://j.db?mode=rwc\"\n{more}");
        let retention = |more: &str| {
            let dir = dir(&[("jetbridge.toml", &jetbridge(more)), ("sessions.toml", SESSIONS)]);
            load(&dir.path().join("jetbridge.toml")).map(|loaded| loaded.retention).map_err(|e| e.to_string())
        };
        assert_eq!(retention(""), Ok(None));
        assert_eq!(retention("retention = \"400d\""), Ok(Some(Duration::from_secs(400 * 86_400))));
        assert_eq!(retention("retention = \"24h\""), Ok(Some(DATABASE_RETENTION_MIN)));
        for (more, error) in [
            ("retention = \"400 days\"", "invalid duration \"400 days\""),
            ("retention = \"0d\"", "retention must be positive"),
            ("retention = \"23h\"", "[database] retention \"23h\": at least 1d"),
            ("retention = \"86399s\"", "[database] retention \"86399s\": at least 1d"),
        ] {
            let error_text = retention(more).unwrap_err();
            assert!(error_text.contains(error), "{more}: {error_text}");
        }
        let path = Path::new("jetbridge.toml");
        let reread = |more: &str| reread(path, &jetbridge(more), &[], false).map(|reread| reread.retention);
        assert_eq!(reread("retention = \"2d\"").unwrap(), Some(Duration::from_secs(2 * 86_400)));
        let error = reread("retention = \"1h\"").unwrap_err().to_string();
        assert!(error.contains("[database] retention \"1h\": at least 1d"), "{error}");
    }

    /// `[message_log]`'s `dir` is relative to the file and required; the rest are optional, checked,
    /// and no other key is taken. Loading opens the log, creating its directory.
    #[test]
    fn the_message_log_section_resolves_its_dir_and_takes_no_other_key() {
        let jetbridge = "sessions = \"sessions.toml\"\n[message_log]\ndir = \"log/fix\"\n";
        let files = dir(&[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS)]);
        let log = load(&files.path().join("jetbridge.toml")).unwrap().message_log.unwrap();
        let expected =
            MessageLog { dir: files.path().join("log/fix"), file_bytes_max: None, retention: None, compress: false };
        assert_eq!(log, expected);
        assert!(files.path().join("log/fix").is_dir());

        let with = |more: &str| {
            let dir = dir(&[("jetbridge.toml", format!("{jetbridge}{more}").as_str()), ("sessions.toml", SESSIONS)]);
            load(&dir.path().join("jetbridge.toml"))
                .map(|loaded| loaded.message_log.unwrap())
                .map_err(|e| e.to_string())
        };
        let log = with("file_bytes_max = 1000\nretention = \"90d\"\n").unwrap();
        assert_eq!(log.file_bytes_max, NonZeroU64::new(1000));
        assert_eq!(log.retention, Some(Duration::from_secs(90 * 86_400)));
        assert_eq!(with("retention = \"12h\"").unwrap().retention, Some(Duration::from_secs(12 * 3_600)));
        for (more, error) in [
            ("retention = \"90 days\"", "invalid duration \"90 days\""),
            ("retention = \"d\"", "invalid duration"),
            ("retention = \"0d\"", "retention must be positive"),
            ("retention = \"999999999999999999d\"", "longer than 100 years"),
            ("retention = \"36601d\"", "longer than 100 years"),
            ("file_bytes_max = 0", "nonzero"),
            ("level = 1", "unknown field `level`"),
        ] {
            let error_text = with(more).unwrap_err();
            assert!(error_text.contains(error), "{more}: {error_text}");
        }
        assert!(load_error("sessions = \"sessions.toml\"\n[message_log]\n").contains("missing field `dir`"));
        let blocked = dir(&[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS), ("log", "a file")]);
        let error = load(&blocked.path().join("jetbridge.toml")).unwrap_err().to_string();
        assert!(error.contains("[message_log] cannot open "), "{error}");
        let compressed = with("compress = true");
        if cfg!(feature = "gzip") {
            assert!(compressed.unwrap().compress);
        } else {
            assert!(compressed.unwrap_err().contains("[message_log] compress: this build can't compress"));
        }
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
        let router = reload(&path, &venues).unwrap().router;
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
        let limits = reload(&path, &venues).unwrap().limits;
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

    /// When the trading day containing `time` (RFC 3339) ends, by `day_end`.
    fn day_ends(day_end: &SessionSchedule, time: &str) -> String {
        let time = time.parse().unwrap();
        day_end.period_at(time).unwrap().end.to_rfc3339()
    }

    /// In the zone's local time, daylight saving or not; the day containing the end is the next.
    #[test]
    fn day_end_is_a_local_time_in_its_zone() {
        let new_york = day_end("17:00 America/New_York").unwrap();
        assert_eq!(day_ends(&new_york, "2026-10-10T20:59:59Z"), "2026-10-10T21:00:00+00:00");
        assert_eq!(day_ends(&new_york, "2026-10-10T21:00:00Z"), "2026-10-11T21:00:00+00:00");
        assert_eq!(day_ends(&new_york, "2026-11-10T12:00:00Z"), "2026-11-10T22:00:00+00:00");
        let offset = day_end("08:30:15 +05:30").unwrap();
        assert_eq!(day_ends(&offset, "2026-10-10T12:00:00Z"), "2026-10-11T03:00:15+00:00");
        assert_eq!(day_end("17:00").unwrap(), day_end("17:00 UTC").unwrap());
    }

    #[test]
    fn day_end_is_midnight_utc_unless_set_and_a_bad_one_is_an_error() {
        let dir = dir(&[("jetbridge.toml", "sessions = \"sessions.toml\""), ("sessions.toml", SESSIONS)]);
        let loaded = load(&dir.path().join("jetbridge.toml")).unwrap();
        assert_eq!(day_ends(&loaded.day_end, "2026-10-10T12:00:00Z"), "2026-10-11T00:00:00+00:00");
        for (day_end, error) in [
            ("", "empty"),
            ("5pm UTC", "invalid time"),
            ("17:00 Mars/Olympus", "unknown time zone"),
            ("17:00 UTC mon-fri", "unexpected \"mon-fri\""),
        ] {
            let error_text = load_error(&format!("sessions = \"sessions.toml\"\nday_end = \"{day_end}\""));
            assert!(error_text.contains("day_end: ") && error_text.contains(error), "{error_text}");
        }
    }

    #[test]
    fn reloading_reads_day_end() {
        let dir = dir(&[("jetbridge.toml", "sessions = \"sessions.toml\"\nday_end = \"17:00\"")]);
        let reloaded = reload(&dir.path().join("jetbridge.toml"), &[]).unwrap().day_end;
        assert_eq!(reloaded, day_end("17:00").unwrap());
    }

    /// `[api]`'s paths are relative to the file; `listen` is required, `audit_retention` optional,
    /// unknown keys are errors.
    #[test]
    fn the_api_section_resolves_its_paths_and_takes_no_other_key() {
        let jetbridge = "sessions = \"sessions.toml\"\n[api]\nlisten = \"127.0.0.1:8080\"\n\
                         users = \"users.toml\"\naudit_dir = \"log/audit\"\n";
        let dir = dir(&[("jetbridge.toml", jetbridge), ("sessions.toml", SESSIONS)]);
        let api = load(&dir.path().join("jetbridge.toml")).unwrap().api.unwrap();
        assert_eq!(api.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(api.users, dir.path().join("users.toml"));
        assert_eq!(api.audit_dir, dir.path().join("log/audit"));
        assert_eq!((api.tls, api.behind_proxy, api.audit_retention), (None, false, None));
        let kept = format!("{jetbridge}audit_retention = \"400d\"\n");
        let files = self::dir(&[("jetbridge.toml", kept.as_str()), ("sessions.toml", SESSIONS)]);
        let api = load(&files.path().join("jetbridge.toml")).unwrap().api.unwrap();
        assert_eq!(api.audit_retention, Some(Duration::from_secs(400 * 86_400)));
        assert!(load_error(&format!("{jetbridge}audit_retention = \"0d\"")).contains("retention must be positive"));
        assert!(load_error(&format!("{jetbridge}port = 1")).contains("unknown field `port`"));
        // The single file the audit log was before its daily files: refused, not ignored.
        assert!(load_error(&format!("{jetbridge}audit = \"audit.jsonl\"")).contains("unknown field `audit`"));
        let without_listen = jetbridge.replace("listen = \"127.0.0.1:8080\"\n", "");
        assert!(load_error(&without_listen).contains("missing field `listen`"));
    }

    /// The bind rule: off loopback, the API needs TLS or a proxy in front saying it terminates TLS.
    #[test]
    fn the_api_listens_off_loopback_only_with_tls_or_behind_a_proxy() {
        let api = |listen: &str, more: &str| {
            let jetbridge = format!(
                "sessions = \"sessions.toml\"\n[api]\nlisten = \"{listen}\"\nusers = \"u\"\naudit_dir = \"a\"\n{more}"
            );
            let dir = dir(&[("jetbridge.toml", &jetbridge), ("sessions.toml", SESSIONS)]);
            load(&dir.path().join("jetbridge.toml")).map(|loaded| loaded.api.unwrap().listen).map_err(|e| e.to_string())
        };
        assert!(api("127.0.0.1:8080", "").is_ok());
        assert!(api("[::1]:8080", "").is_ok());
        let error = api("0.0.0.0:8080", "").unwrap_err();
        assert!(error.contains("[api] listen = \"0.0.0.0:8080\" isn't a loopback address"), "{error}");
        assert!(api("[::ffff:127.0.0.1]:8080", "").is_ok());
        assert!(api("[::]:8080", "").unwrap_err().contains("isn't a loopback address"));
        assert!(api("0.0.0.0:8080", "behind_proxy = true").is_ok());
        let tls = api("0.0.0.0:8080", "tls = { cert = \"c.pem\", key = \"k.pem\" }");
        if cfg!(feature = "tls") {
            assert!(tls.is_ok(), "{tls:?}");
        } else {
            assert!(tls.unwrap_err().contains("this build has no TLS"));
        }
    }

    /// The example files the readme shows load.
    #[test]
    fn the_examples_load() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/jetbridge.toml");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.version, Version::Fix44);
        assert_eq!(loaded.sessions.initiator_names(), ["NASDAQ"]);
        let funda = loaded.limits.limits("FUNDA");
        assert_eq!(funda.max_notional, Some(decimal("2500000.50")));
        assert_eq!((funda.max_order_qty, funda.price_band), (Some(decimal("10000")), Some(decimal("5"))));
        assert_eq!(loaded.limits.limits("FUNDB").credit, None);

        // The commented `[message_log]`, uncommented, is valid too.
        let text = std::fs::read_to_string(&path).unwrap();
        let (before, example) = text.split_once("# [message_log]").unwrap();
        let mut uncommented = format!("{before}[message_log]");
        for line in example.lines() {
            uncommented.push_str(line.strip_prefix("# ").unwrap_or(line));
            uncommented.push('\n');
        }
        let reread = reread(&path, &uncommented, &["NASDAQ".to_string()], false);
        if cfg!(feature = "gzip") {
            let log = reread.unwrap().message_log.unwrap();
            assert_eq!(log.dir, path.parent().unwrap().join("log"));
            assert_eq!((log.retention, log.compress), (Some(Duration::from_secs(90 * 86_400)), true));
        } else {
            assert!(reread.unwrap_err().to_string().contains("build with the `gzip` feature"));
        }
    }
}
