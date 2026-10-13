//! The gateway running from its configuration files: the acceptor serving clients, an initiator
//! per venue, the operator API if configured, and reloading. The binary adds arguments and
//! signals.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};
use turbojet::{Acceptor, SessionEvent, SessionHandle, SessionId, SessionSchedule, SessionStatus};
use turbojet_config::{Changes, Initiators, SessionsFile};

use crate::api;
use crate::book::{self, Snapshot};
use crate::config::{self, Error, RiskTable, RiskTables};
use crate::edge::{ClientApp, Gateway, Schedules, TICK_INTERVAL, VenueApp, Writer};
use crate::fix::Version;
use crate::limits::{self, EditError};
use crate::risk;
use crate::router::Router;
use crate::store::Store;

/// The Text(58) of the Logouts sent at shutdown.
const SHUTTING_DOWN: &str = "gateway shutting down";

/// How long shutdown waits, with the clients gone, for the venues to answer the gateway's
/// cancels (those cancel on disconnect sends as the clients log out, say). An order left working
/// at a venue with no gateway to report its fills is worse than a slower stop; a venue that
/// doesn't answer in this long isn't going to soon.
const CANCELS_WAIT: Duration = Duration::from_secs(5);

/// How often the database is pruned, with `[database] retention`, the first time this long after
/// the start: a start isn't slowed, and its orphans are found by then (see
/// [`Gateway::orphans_pending`]). Orders become prunable a trading day at a time, so an hour late
/// keeps them an hour longer, no more.
const PRUNE_INTERVAL: Duration = Duration::from_secs(3_600);

/// What [`Service::reload`] did with each file: what changed, or why the file was kept as it
/// was; and what it took but warns about.
#[derive(Debug)]
pub struct Reloaded {
    pub sessions: Result<Changes, String>,
    /// The routes, limits and `day_end`, taken together or not at all.
    pub config: Result<ConfigSummary, String>,
    /// Problems with what was taken: a sessions file that, with a database, keeps a session's
    /// messages only in memory, so a restart loses what recovery needs.
    pub warnings: Vec<String>,
}

/// What [`Service::edit_limits`] did.
#[derive(Debug)]
pub struct Edited {
    /// The table edited, as the file now has it; `None` if it has none.
    pub table: Option<RiskTable>,
    pub reloaded: Reloaded,
}

/// The routes, limits and `day_end` a reload took.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigSummary {
    pub routes: usize,
    /// CompIDs with limits of their own.
    pub clients: usize,
    pub day_end: String,
}

/// The gateway, serving.
#[derive(Debug)]
pub struct Service {
    path: PathBuf,
    gateway: Arc<Gateway>,
    /// Shared with the gateway, which reads the clients' schedules from it.
    sessions: Arc<SessionsFile>,
    acceptor: Acceptor,
    initiators: Initiators,
    local_addr: SocketAddr,
    /// The `[database] url` the gateway started with: a reload doesn't change it.
    database: Option<String>,
    /// The `[message_log]` the gateway started with, its log held by the sessions file: a reload
    /// doesn't change it.
    message_log: Option<config::MessageLog>,
    /// [`Gateway::run_ticks`], aborted at shutdown, or when the service is dropped.
    retrying: JoinHandle<()>,
    /// The book's writer, with a database: aborted when the service is dropped.
    writing: Option<JoinHandle<()>>,
    /// [`prune_hourly`], with a database: aborted at shutdown, or when the service is dropped.
    pruning: Option<JoinHandle<()>>,
    /// The `[database] retention` in force: set at start and by each reload, read by `pruning`.
    retention: Arc<Mutex<Option<Duration>>>,
    /// The database, if there is one.
    store: Option<Store>,
    /// The operator API, with `[api]`: set once the acceptor serves, taken at shutdown.
    api: Mutex<Option<api::Server>>,
    /// The limits in force, as the file writes them: set with the gateway's.
    risk: Mutex<RiskTables>,
    /// Held for the whole of a reload: SIGHUP and the API can each start one, and only the
    /// sessions file's half locks itself, so an older jetbridge.toml could land over a newer one.
    reloading: Mutex<()>,
    /// Set as shutdown starts: a reload that hasn't begun doesn't, as it would start initiators
    /// being stopped.
    stopping: AtomicBool,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.retrying.abort();
        for task in [&self.writing, &self.pruning].into_iter().flatten() {
            task.abort();
        }
        if let Ok(Some(api)) = self.api.get_mut() {
            api.abort();
        }
    }
}

/// The acceptor's serving task; it ends only if accepting fails.
pub type Serving = JoinHandle<io::Result<()>>;

impl Service {
    /// Loads the configuration file at `path`, starts the venues' initiators, serves the clients'
    /// acceptor, and then the operator API, with `[api]`. Call it within a tokio runtime.
    ///
    /// # Errors
    ///
    /// The configuration doesn't load, the database can't be opened, the acceptor's or the API's
    /// address can't be bound, a venue speaks a FIX version the gateway doesn't convert, or TLS or
    /// a venue can't be set up.
    pub async fn start(path: &Path) -> Result<(Arc<Self>, Serving), Error> {
        Self::start_with(path, StartOptions::default()).await
    }

    /// As [`start`](Self::start), with `options`.
    pub(crate) async fn start_with(path: &Path, options: StartOptions) -> Result<(Arc<Self>, Serving), Error> {
        let StartOptions { stores, listener, api_listener, max_commit_wait } = options;
        let loaded = config::load(path, stores)?;
        if loaded.database.is_some() {
            check_durable(&loaded.sessions)?;
        }
        // First, so a database that won't open stops nothing half-started.
        let (database, writing) = match &loaded.database {
            Some(url) => {
                let (store, snapshot, writer, writing) = start_writer(url, max_commit_wait).await?;
                (Some((store, snapshot, writer)), Some(writing))
            }
            None => (None, None),
        };
        let sessions = Arc::new(loaded.sessions);
        let store = database.as_ref().map(|(store, ..)| store.clone());
        let gateway =
            start_gateway(loaded.router, loaded.version, loaded.limits, loaded.day_end, &sessions, database).await?;
        // Bound before the venues start, so a port in use stops nothing half-started.
        let (listener, local_addr) = listen(&sessions, listener).await?;
        let api = match &loaded.api {
            Some(config) => Some(api::Bound::bind(config, api_listener).await?),
            None => None,
        };
        let initiators = sessions.initiators(Arc::new(VenueApp(gateway.clone()))).map_err(Error::new)?;
        let acceptor = sessions.acceptor(Arc::new(ClientApp(gateway.clone()))).expect("load checked [acceptor]");
        let retrying = tokio::spawn(gateway.clone().run_ticks(TICK_INTERVAL));
        let retention = Arc::new(Mutex::new(loaded.retention));
        let pruning = store.clone().map(|store| tokio::spawn(prune_hourly(gateway.clone(), store, retention.clone())));
        let service = Self {
            path: path.to_path_buf(),
            gateway,
            sessions,
            acceptor,
            initiators,
            local_addr,
            database: loaded.database,
            message_log: loaded.message_log,
            retrying,
            writing,
            pruning,
            retention,
            store,
            api: Mutex::new(None),
            risk: Mutex::new(loaded.risk),
            reloading: Mutex::new(()),
            stopping: AtomicBool::new(false),
        };
        service.set_venues();
        // The initiators are running: stop them rather than leave them logging on.
        let serving = match service.check_versions().and_then(|()| service.serve(listener)) {
            Ok(serving) => serving,
            Err(e) => {
                service.initiators.shutdown(None).await;
                return Err(e);
            }
        };
        info!(
            addr = %local_addr,
            version = ?loaded.version,
            venues = ?service.initiators.names(),
            id_prefix = %service.gateway.id_prefix(),
            "gateway listening"
        );
        let service = Arc::new(service);
        if let Some(api) = api {
            *service.api.lock().expect("API lock poisoned") = Some(api.serve(Arc::downgrade(&service)));
        }
        Ok((service, serving))
    }

    /// The address the acceptor listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The address the operator API listens on, while it serves.
    pub fn api_addr(&self) -> Option<SocketAddr> {
        self.api.lock().expect("API lock poisoned").as_ref().map(api::Server::local_addr)
    }

    /// The gateway: the book and where its outputs go.
    pub fn gateway(&self) -> &Arc<Gateway> {
        &self.gateway
    }

    /// The message log's directory, with `[message_log]`: what the API searches.
    pub fn message_log_dir(&self) -> Option<&Path> {
        self.message_log.as_ref().map(|log| log.dir.as_path())
    }

    /// The book's database, if there is one: what the API reads past orders from.
    pub(crate) fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// The limits in force, as the file writes them.
    pub fn risk_tables(&self) -> RiskTables {
        self.risk.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The client session `id`, connected or not.
    pub fn client_session(&self, id: &SessionId) -> SessionHandle {
        self.acceptor.handle(id)
    }

    /// The client sessions connected now: see [`Acceptor::statuses`].
    pub fn client_statuses(&self) -> Vec<SessionStatus> {
        self.acceptor.statuses()
    }

    /// The client sessions an operator has paused, connected or not.
    pub fn client_paused(&self) -> Vec<SessionId> {
        self.acceptor.paused()
    }

    /// What happens to the client sessions from now on: see [`Acceptor::subscribe`].
    pub fn client_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.acceptor.subscribe()
    }

    /// What happens to the venues' sessions from now on, those a reload starts later included:
    /// see [`Initiators::subscribe`].
    pub fn venue_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.initiators.subscribe()
    }

    /// The venues whose initiators are running, by name (`[initiator.NAME]`), in order.
    pub fn venues(&self) -> Vec<String> {
        self.initiators.names()
    }

    /// The session of venue `name` (its `[initiator.NAME]`), if its initiator is running.
    pub fn venue_session(&self, name: &str) -> Option<SessionHandle> {
        self.initiators.handle(name)
    }

    /// Rereads the sessions file, the routes, limits, `day_end`, the API's certificate and its
    /// users file, logging what changed.
    /// Each file that doesn't load is logged and stays as it was (routes, limits and `day_end`
    /// together; a users file that doesn't load is a warning); the routes are checked against the
    /// venues the gateway took after the sessions reload, so a venue added to both files at once
    /// can be routed to. A changed schedule or `day_end` applies from each session's next trading
    /// day. A changed `[message_log]` is a warning: the log in use is kept until a restart.
    /// Returns what it logged: each file's changes or why it was kept, and any warning about what
    /// was taken. `None`, doing nothing, once shutdown has started.
    ///
    /// One reload runs at a time, the others waiting: call it off the async workers
    /// (`spawn_blocking`).
    pub fn reload(&self) -> Option<Reloaded> {
        // Nothing panics while holding it but a reload's own bug; the next reload goes ahead.
        let _reloading = self.reloading.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopping.load(Ordering::SeqCst) {
            warn!("not reloading: the gateway is stopping");
            return None;
        }
        Some(self.reload_locked())
    }

    /// Edits the risk limits in the configuration file and reloads, as [`reload`](Self::reload)
    /// does, all under its lock: reads the file afresh, makes `edit` (see [`limits::apply`]),
    /// checks the whole text as a reload would, and only then writes it, atomically (to the file
    /// itself, if the path is a symlink), keeping its permissions. A file that doesn't load as it
    /// is refuses every edit until it's fixed. Returns the table the edit is to, as the file now
    /// has it (`None` once deleted), and what the reload did, which takes whatever else changed
    /// in the files since the last.
    ///
    /// Call it off the async workers (`spawn_blocking`), as [`reload`](Self::reload).
    ///
    /// # Errors
    ///
    /// The gateway is stopping, the edit can't be made or the file wouldn't load with it (it's
    /// left as it was), or the file can't be read or written.
    pub fn edit_limits(&self, edit: &limits::Edit) -> Result<Edited, EditError> {
        let _reloading = self.reloading.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopping.load(Ordering::SeqCst) {
            return Err(EditError::Stopping);
        }
        let io = |e: io::Error| EditError::Io(format!("{}: {e}", self.path.display()));
        let text = std::fs::read_to_string(&self.path).map_err(io)?;
        let edited = limits::apply(&text, edit).map_err(|e| match e {
            EditError::Refused(why) => EditError::Refused(format!("{}: {why}", self.path.display())),
            e => e,
        })?;
        let venues = self.set_venues();
        // Quiet: the reload below logs what's worth a warning.
        let reread =
            config::reread(&self.path, &edited, &venues, false).map_err(|e| EditError::Refused(e.to_string()))?;
        let warning = crate::auth::write_atomically(&self.path, &edited).map_err(io)?;
        info!(file = %self.path.display(), "edited the risk limits: {edit}");
        let mut reloaded = self.reload_locked();
        reloaded.warnings.extend(warning);
        Ok(Edited { table: edit.table_in(&reread.risk), reloaded })
    }

    /// [`reload`](Self::reload)'s work, its lock held.
    fn reload_locked(&self) -> Reloaded {
        let sessions = self.sessions.reload_all(Some(&self.acceptor), &self.initiators).map_err(|e| e.to_string());
        match &sessions {
            Ok(changes) => {
                info!(
                    added = ?changes.added,
                    changed = ?changes.changed,
                    removed = ?changes.removed,
                    logged_out = ?changes.logged_out,
                    started = ?changes.started,
                    reconfigured = ?changes.reconfigured,
                    stopped = ?changes.stopped,
                    "reloaded the sessions file"
                );
            }
            Err(e) => warn!("keeping the sessions file in use: {e}"),
        }
        let mut warnings = Vec::new();
        warnings.extend(self.reload_users());
        if self.database.is_some()
            && let Err(e) = check_durable(&self.sessions)
        {
            let warning = format!("the reloaded sessions file loses what a restart needs: {e}");
            error!("{warning}");
            warnings.push(warning);
        }
        let venues = self.set_venues();
        let config = match config::reload(&self.path, &venues) {
            Ok(config::Reread { router, limits, risk, day_end, api, message_log, database, retention }) => {
                let summary = ConfigSummary {
                    routes: router.rules().len(),
                    clients: limits.by_comp_id.len(),
                    day_end: day_end.to_string(),
                };
                info!(
                    routes = summary.routes,
                    clients = summary.clients,
                    day_end = %summary.day_end,
                    "reloaded the routes, limits and day_end"
                );
                self.gateway.set_config(router, limits);
                *self.risk.lock().unwrap_or_else(PoisonError::into_inner) = risk;
                self.gateway.set_day_end(day_end);
                *self.retention.lock().unwrap_or_else(PoisonError::into_inner) = retention;
                warnings.extend(self.reload_api(api.as_ref()));
                warnings.extend(self.needing_restart(message_log.as_ref(), database.as_deref()));
                Ok(summary)
            }
            Err(e) => {
                warn!("keeping the routes, limits and day_end in use: {e}");
                Err(e.to_string())
            }
        };
        Reloaded { sessions, config, warnings }
    }

    /// Rereads the API's users file, returning (and logging) why it was kept, if it was. Whatever
    /// else the reload refuses: the users file stands alone.
    fn reload_users(&self) -> Option<String> {
        let warning = self.api.lock().expect("API lock poisoned").as_ref()?.reload_users()?;
        warn!("{warning}");
        Some(warning)
    }

    /// What changed in the reloaded file but waits for a restart, logged: the `[message_log]` and
    /// the `[database] url`, which the gateway keeps as it started.
    fn needing_restart(&self, message_log: Option<&config::MessageLog>, database: Option<&str>) -> Vec<String> {
        let mut warnings = Vec::new();
        if message_log != self.message_log.as_ref() {
            warnings.push("[message_log] changed, which needs a restart: the log in use is kept".to_string());
        }
        if database != self.database.as_deref() {
            warnings.push("[database] url changed, which needs a restart: the database in use is kept".to_string());
        }
        for warning in &warnings {
            warn!(file = %self.path.display(), "{warning}");
        }
        warnings
    }

    /// Takes the reloaded `[api]`'s certificate, returning (and logging) what waits for a restart.
    fn reload_api(&self, config: Option<&config::Api>) -> Vec<String> {
        let warnings = match &*self.api.lock().expect("API lock poisoned") {
            Some(server) => server.reload(config),
            None if config.is_some() => vec!["[api] added: the API starts at the next start".to_string()],
            None => Vec::new(),
        };
        for warning in &warnings {
            warn!("{warning}");
        }
        warnings
    }

    /// Waits for a reload under way and refuses later ones, stops pruning, stops the operator API
    /// (its requests under way finish first, briefly), logs the clients' sessions out, cancels the
    /// working orders of those with cancel on disconnect (pending countdowns fire as the clients
    /// go; the shutdown's own Logouts start none), waits for the venues to answer the gateway's
    /// cancels, logs the venues' sessions out, returning once all have closed, then logs what the
    /// stop loses. The message log writes what it holds and finishes its last file once the service is
    /// dropped (with the sessions, which hold it too).
    pub async fn shutdown(&self) {
        // A reload under way finishes first; any later one finds the flag and does nothing.
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(pruning) = &self.pruning {
            pruning.abort();
        }
        // ponytail: polled, as the cancels below are; reloads are rare and quick.
        while matches!(self.reloading.try_lock(), Err(TryLockError::WouldBlock)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let api = self.api.lock().expect("API lock poisoned").take();
        if let Some(api) = api {
            api.shutdown().await;
        }
        self.acceptor.shutdown(Some(SHUTTING_DOWN)).await;
        self.gateway.cancel_at_shutdown(|comp_id| {
            self.sessions.counterparty_config(comp_id).is_some_and(|config| config.cancel_on_disconnect.is_some())
        });
        // The retry tick keeps running meanwhile, so cancels a venue's session couldn't take go.
        let settled = async {
            // ponytail: polled; a notify from the book if shutdown latency ever matters.
            while !self.gateway.cancels_settled() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        if tokio::time::timeout(CANCELS_WAIT, settled).await.is_err() {
            warn!(cancelling = ?self.gateway.cancelling(), "stopping without the venues' answer to these cancels");
        }
        self.retrying.abort();
        self.initiators.shutdown(Some(SHUTTING_DOWN)).await;
        // What the last messages changed, before the process goes. A database that's down holds
        // this up; a second signal exits (see the binary).
        self.gateway.committed().await;
        // Open orders stay open at their venues with no gateway to report to until a restart,
        // which restores them and the undelivered reports from the database, if there is one.
        let unfinished = self.gateway.unfinished();
        if unfinished.open_orders.is_empty() && unfinished.undelivered.is_empty() {
            info!("no open orders or undelivered reports");
        } else {
            warn!(
                open_orders = ?unfinished.open_orders,
                undelivered = ?unfinished.undelivered,
                gateway_cancels = ?unfinished.cancelling,
                "stopping with open orders (per venue), undelivered reports (per client) and the \
                 gateway's own cancels still going (stuck ones given up)"
            );
        }
    }

    /// Checks that the gateway converts every venue's FIX version. Only at start: turbojet-config
    /// doesn't say an initiator's BeginString until it's running, and a reload skips such a venue
    /// with an error logged rather than stop the others (see [`Gateway::set_venues`]).
    fn check_versions(&self) -> Result<(), Error> {
        for name in self.initiators.names() {
            let id = self.initiators.session_id(&name).expect("a running initiator");
            if let Err(e) = id.begin_string.parse::<Version>() {
                return Err(Error::new(format!("venue {name:?}: {e}")));
            }
        }
        Ok(())
    }

    /// Tells the gateway the venues running now (a reload may have started or stopped some), and
    /// returns those it took.
    fn set_venues(&self) -> Vec<String> {
        let names = self.initiators.names();
        let venues = names.iter().filter_map(|name| Some((name.as_str().into(), self.initiators.handle(name)?)));
        let skipped = self.gateway.set_venues(venues);
        names.into_iter().filter(|name| !skipped.iter().any(|s| s == name.as_str())).collect()
    }

    #[cfg(feature = "tls")]
    fn serve(&self, listener: TcpListener) -> Result<Serving, Error> {
        let tls = self.sessions.server_tls().map_err(|e| Error::new(format!("cannot set up TLS: {e}")))?;
        let acceptor = self.acceptor.clone();
        Ok(match tls {
            Some(tls) => tokio::spawn(acceptor.serve_tls(listener, tls.acceptor())),
            None => tokio::spawn(acceptor.serve(listener)),
        })
    }

    /// Without the `tls` feature, a sessions file with TLS doesn't load.
    #[cfg(not(feature = "tls"))]
    #[expect(clippy::unnecessary_wraps, reason = "the same signature as with TLS")]
    fn serve(&self, listener: TcpListener) -> Result<Serving, Error> {
        Ok(tokio::spawn(self.acceptor.clone().serve(listener)))
    }
}

/// The gateway, with the file's routes, `limits` and `day_end`, the clients' schedules from
/// `sessions`, and the book restored from `database`, if given, its orphaned venue requests
/// cancelled.
async fn start_gateway(
    router: Router,
    version: Version,
    limits: risk::Config,
    day_end: SessionSchedule,
    sessions: &Arc<SessionsFile>,
    database: Option<(Store, Snapshot, Writer)>,
) -> Result<Arc<Gateway>, Error> {
    let prefix = book::start_prefix(next_second().await);
    let schedules = {
        let sessions = sessions.clone();
        Schedules(Arc::new(move |comp_id| sessions.counterparty_config(comp_id)?.schedule))
    };
    let gateway =
        Gateway::new(router, version).with_id_prefix(prefix).with_limits(limits).with_days(day_end, schedules);
    let (gateway, recovery) = match database {
        Some((store, snapshot, writer)) => (gateway.with_writer(writer), Some((store, snapshot))),
        None => (gateway, None),
    };
    let gateway = Arc::new(gateway);
    // Before any session starts: the clients' sessions find their orders and reports, and the
    // venues' stores are read before their initiators open them.
    if let Some((store, snapshot)) = recovery {
        gateway.restore(snapshot);
        let venues = sessions.initiator_stores();
        gateway
            .cancel_orphans(&store, venues)
            .await
            .map_err(|e| Error::new(format!("cannot look for orphaned venue requests: {e}")))?;
    }
    Ok(gateway)
}

/// Every [`PRUNE_INTERVAL`] from an interval after the start, prunes `store` by the `retention`
/// in force then: see [`Gateway::prune_once`].
async fn prune_hourly(gateway: Arc<Gateway>, store: Store, retention: Arc<Mutex<Option<Duration>>>) {
    let mut interval = tokio::time::interval(PRUNE_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick is at once.
    interval.tick().await;
    loop {
        interval.tick().await;
        let retention = *retention.lock().unwrap_or_else(PoisonError::into_inner);
        gateway.prune_once(&store, retention).await;
    }
}

/// The clients' listener: `given`, or one bound to the sessions file's address.
async fn listen(
    sessions: &SessionsFile,
    given: Option<std::net::TcpListener>,
) -> Result<(TcpListener, SocketAddr), Error> {
    let listen = sessions.listen().expect("load checked the file has [acceptor]");
    let listener = match given {
        Some(listener) => listener
            .set_nonblocking(true)
            .and_then(|()| TcpListener::from_std(listener))
            .map_err(|e| Error::new(format!("cannot listen on the listener given: {e}")))?,
        None => TcpListener::bind(&listen).await.map_err(|e| Error::new(format!("cannot bind {listen}: {e}")))?,
    };
    let local_addr = listener.local_addr().map_err(|e| Error::new(format!("{listen}: {e}")))?;
    Ok((listener, local_addr))
}

/// The start of the next second: the time a start takes its ID prefix from (see
/// [`book::start_prefix`]). A start that began in the second another took its prefix in, its
/// process having stopped since, takes a later one, so no two starts share a prefix (unless the
/// clock goes back), and every ID a start issues, its rejects' ExecIDs too, is its own.
async fn next_second() -> SystemTime {
    let since_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    tokio::time::sleep(
        Duration::from_secs(1).saturating_sub(Duration::from_nanos(u64::from(since_epoch.subsec_nanos()))),
    )
    .await;
    loop {
        let now = SystemTime::now();
        let second = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        if second > since_epoch.as_secs() {
            return now;
        }
        // A sleep that woke a little early.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// What [`Service::start_with`] does differently from [`Service::start`].
#[derive(Default)]
pub(crate) struct StartOptions {
    /// Stores the sessions file may name, registered by name (see [`config::load`]).
    pub(crate) stores: Vec<(String, Arc<dyn turbojet::SessionStorage>)>,
    /// Serves clients on this rather than binding the file's address: a listener kept open across
    /// starts, as socket activation keeps one, holds its port while the gateway is down.
    pub(crate) listener: Option<std::net::TcpListener>,
    /// Serves the API on this rather than binding `[api] listen`.
    pub(crate) api_listener: Option<std::net::TcpListener>,
    /// The writer's [`MAX_COMMIT_WAIT`](crate::edge::writer::MAX_COMMIT_WAIT) instead.
    pub(crate) max_commit_wait: Option<Duration>,
}

/// With a database, every session must outlive the process: a client's or venue's session kept
/// in memory forgets the messages the book's state assumes were stored.
fn check_durable(sessions: &SessionsFile) -> Result<(), Error> {
    if sessions.acceptor_store().is_some_and(|store| !store.is_durable()) {
        return Err(Error::new(
            "with a [database], the clients' sessions must be kept in a durable store (kind = \"disk\", say): \
             each counterparty's, and [defaults]' under unknown = \"admit\"",
        ));
    }
    match sessions.initiator_stores().into_iter().find(|(.., store)| !store.is_durable()) {
        Some((venue, ..)) => Err(Error::new(format!(
            "with a [database], venue {venue}'s session must be kept in a durable store (kind = \"disk\", say)"
        ))),
        None => Ok(()),
    }
}

/// Opens the database at `url`, loads what the book restores, and starts the book's writer on it,
/// rejecting new orders after `max_commit_wait` if given.
async fn start_writer(
    url: &str,
    max_commit_wait: Option<Duration>,
) -> Result<(Store, Snapshot, Writer, JoinHandle<()>), Error> {
    let store = Store::open(url).await.map_err(|e| Error::new(e.to_string()))?;
    let snapshot = store.load().await.map_err(|e| Error::new(format!("cannot load the database: {e}")))?;
    let (mut writer, writing) = Writer::start(store.clone(), snapshot.next_undelivered_id());
    if let Some(max_wait) = max_commit_wait {
        writer = writer.with_max_wait(max_wait);
    }
    Ok((store, snapshot, writer, writing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{Router, Rule, Target};

    const ACCEPTOR: &str = "[acceptor]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"JETBRIDGE\"\n";

    /// An initiator section for venue `name`, speaking `begin_string`, retrying a closed port.
    fn venue_section(name: &str, begin_string: &str) -> String {
        format!(
            "[initiator.{name}]\nbegin_string = \"{begin_string}\"\ntarget_comp_id = \"{name}\"\n\
             connect = [\"127.0.0.1:1\"]\n"
        )
    }

    fn routes_to(venue: &str) -> String {
        format!("sessions = \"sessions.toml\"\n[[route]]\nvenue = \"{venue}\"\n")
    }

    fn write(dir: &tempfile::TempDir, name: &str, text: &str) {
        std::fs::write(dir.path().join(name), text).unwrap();
    }

    /// Starts from the example files on an ephemeral port, with the venue's initiator retrying a
    /// closed port, and shuts down. Orders flowing end to end are `tests/e2e.rs`'s.
    #[tokio::test]
    async fn starts_from_its_files_and_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/sessions.toml"))
            .unwrap()
            .replace("127.0.0.1:9876", "127.0.0.1:0")
            .replace("127.0.0.1:9877", "127.0.0.1:1");
        write(&dir, "sessions.toml", &sessions);
        let jetbridge = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/jetbridge.toml"))
            .unwrap()
            .replace("127.0.0.1:8080", "127.0.0.1:0");
        write(&dir, "jetbridge.toml", &jetbridge);
        let users = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/users.toml")).unwrap();
        write(&dir, "users.toml", &users);
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        assert_ne!(service.local_addr().port(), 0);
        assert!(tokio::net::TcpStream::connect(service.local_addr()).await.is_ok());
        assert!(service.api_addr().is_some());
        service.shutdown().await;
        serving.abort();
    }

    /// An `[api]` but its `listen`; with it, the test writes `users.toml`.
    const API: &str = "[api]\nusers = \"users.toml\"\naudit_dir = \"audit\"\n";

    /// The API's users file must load at start; on reload, one that doesn't load keeps the users
    /// in use, with a warning, whatever happens to the rest.
    #[tokio::test]
    async fn the_users_file_loads_at_start_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{API}listen = \"127.0.0.1:0\"\n"));
        let path = dir.path().join("jetbridge.toml");
        let e = Service::start(&path).await.unwrap_err();
        assert!(e.to_string().contains("cannot read"), "{e}");
        write(&dir, "users.toml", "[user.a]\nrole = \"viewer\"\n");
        let e = Service::start(&path).await.unwrap_err();
        assert!(e.to_string().contains("users.toml"), "{e}");
        write(&dir, "users.toml", "");
        let (service, serving) = Service::start(&path).await.unwrap();
        assert!(service.reload().unwrap().warnings.is_empty());
        write(&dir, "users.toml", "[user.a]\nrole = \"viewer\"\n");
        write(&dir, "jetbridge.toml", "not toml");
        let reloaded = service.reload().unwrap();
        assert!(reloaded.config.is_err());
        let [warning] = reloaded.warnings.as_slice() else { panic!("{reloaded:?}") };
        assert!(warning.contains("keeping the users in use"), "{warning}");
        service.shutdown().await;
        serving.abort();
    }

    /// A limit edit through a symlinked path writes the file it points to, keeping the link and
    /// the file's permissions; edits from many threads at once each land; once stopping, an edit
    /// is refused.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn limit_edits_write_through_a_symlink_one_at_a_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "real.toml", "sessions = \"sessions.toml\"\n");
        let (real, link) = (dir.path().join("real.toml"), dir.path().join("jetbridge.toml"));
        std::os::unix::fs::symlink("real.toml", &link).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let (service, serving) = Service::start(&link).await.unwrap();
        let edits: Vec<_> = (0..8)
            .map(|n| {
                let (service, credit) = (service.clone(), serde_json::json!({ "credit": n + 1 }));
                std::thread::spawn(move || {
                    let edit =
                        limits::Edit::set(limits::Limits::Client(format!("FUND{n}")), credit.as_object().unwrap());
                    service.edit_limits(&edit.unwrap()).unwrap()
                })
            })
            .collect();
        for edit in edits {
            assert!(edit.join().unwrap().reloaded.config.is_ok());
        }
        assert_eq!(service.risk_tables().client.len(), 8);
        assert_eq!(service.gateway().limits().by_comp_id.len(), 8);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(std::fs::read_to_string(&real).unwrap().contains("[risk.client.FUND7]\ncredit = 8\n"));

        service.stopping.store(true, Ordering::SeqCst);
        let edit = limits::Edit::Delete { comp_id: "FUND0".into() };
        assert_eq!(service.edit_limits(&edit).unwrap_err(), EditError::Stopping);
        service.shutdown().await;
        serving.abort();
    }

    /// One reload waits for another to finish, so the older file can't land over the newer; and
    /// shutdown waits for one under way.
    #[tokio::test]
    async fn reloads_run_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "jetbridge.toml", "sessions = \"sessions.toml\"\n");
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        let held = service.reloading.lock().unwrap();
        let reloading = service.clone();
        let reload = tokio::task::spawn_blocking(move || reloading.reload());
        // A blocking sleep: the guard can't be held across an await.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!reload.is_finished());
        drop(held);
        assert!(reload.await.unwrap().unwrap().config.is_ok());

        // Shutdown waits for a reload under way (one holding the lock), and refuses later ones.
        let (reloading, finished) = (service.clone(), Arc::new(AtomicBool::new(false)));
        let finishing = finished.clone();
        tokio::task::spawn_blocking(move || {
            let _held = reloading.reloading.lock().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            finishing.store(true, Ordering::SeqCst);
        });
        while !matches!(service.reloading.try_lock(), Err(TryLockError::WouldBlock)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        service.shutdown().await;
        assert!(finished.load(Ordering::SeqCst));
        assert!(service.reload().is_none());
        serving.abort();
    }

    /// The audit log's directory is opened, or created, at start: one that can't be stops the start.
    #[tokio::test]
    async fn the_audit_log_opens_at_start() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "users.toml", "");
        let config = API.replace("\"audit\"", "\"blocked/audit\"");
        write(&dir, "blocked", "");
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{config}listen = \"127.0.0.1:0\"\n"));
        let e = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap_err();
        assert!(e.to_string().contains("cannot open the audit log"), "{e}");
        std::fs::remove_file(dir.path().join("blocked")).unwrap();
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        assert!(dir.path().join("blocked/audit").is_dir());
        service.shutdown().await;
        serving.abort();
    }

    /// The API answers health over a real socket, and shutdown stops it first.
    #[tokio::test]
    async fn the_api_answers_health_until_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{API}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "users.toml", "");
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        let addr = service.api_addr().unwrap();
        let response = api::tests::request(addr, "GET", "/api/health").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("content-type: application/json"), "{response}");
        let version = env!("CARGO_PKG_VERSION");
        assert!(response.ends_with(&format!("{{\"version\":\"{version}\"}}")), "{response}");
        service.shutdown().await;
        assert_eq!(service.api_addr(), None);
        serving.abort();
    }

    /// `behind_proxy = true` lets the API listen off loopback without TLS (here the address is
    /// the file's; the test serves on a loopback listener given instead). A reload asking for a
    /// different address warns that it waits for a restart.
    #[tokio::test]
    async fn the_api_serves_behind_a_proxy_off_loopback() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        let config =
            |listen: &str| format!("sessions = \"sessions.toml\"\n{API}listen = \"{listen}\"\nbehind_proxy = true\n");
        write(&dir, "jetbridge.toml", &config("0.0.0.0:8080"));
        write(&dir, "users.toml", "");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let options = StartOptions { api_listener: Some(listener), ..StartOptions::default() };
        let (service, serving) = Service::start_with(&dir.path().join("jetbridge.toml"), options).await.unwrap();
        assert!(api::tests::request(addr, "GET", "/api/health").await.starts_with("HTTP/1.1 200"));
        assert!(service.reload().unwrap().warnings.is_empty());
        write(&dir, "jetbridge.toml", &config("0.0.0.0:8081"));
        let reloaded = service.reload().unwrap();
        assert!(reloaded.config.is_ok(), "{reloaded:?}");
        let [warning] = reloaded.warnings.as_slice() else { panic!("{reloaded:?}") };
        assert!(warning.contains("[api] changed"), "{warning}");
        service.shutdown().await;
        serving.abort();
    }

    /// With `[api] tls` the API serves HTTPS. A reload whose certificate doesn't load keeps the one
    /// in use, with a warning; one that does presents the renewed certificate.
    #[cfg(feature = "tls")]
    #[tokio::test]
    async fn the_api_serves_over_tls_and_a_reload_renews_its_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = api::tests::localhost_cert();
        write(&dir, "api.pem", &cert);
        write(&dir, "api.key", &key);
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        let tls = "tls = { cert = \"api.pem\", key = \"api.key\" }\n";
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{API}listen = \"127.0.0.1:0\"\n{tls}"));
        write(&dir, "users.toml", "");
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        let addr = service.api_addr().unwrap();
        let health = async |cert: &str| api::tests::request_tls(addr, cert, "/api/health").await;
        let response = health(&cert).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"version\""), "{response}");

        write(&dir, "api.pem", "not a certificate");
        let reloaded = service.reload().unwrap();
        let [warning] = reloaded.warnings.as_slice() else { panic!("{reloaded:?}") };
        assert!(warning.contains("keeping the API's certificate in use"), "{warning}");
        assert!(health(&cert).await.unwrap().starts_with("HTTP/1.1 200"));

        let (renewed, renewed_key) = api::tests::localhost_cert();
        write(&dir, "api.pem", &renewed);
        write(&dir, "api.key", &renewed_key);
        let reloaded = service.reload().unwrap();
        assert!(reloaded.warnings.is_empty(), "{reloaded:?}");
        assert!(health(&cert).await.is_err());
        assert!(health(&renewed).await.unwrap().starts_with("HTTP/1.1 200"));
        service.shutdown().await;
        serving.abort();
    }

    /// A reload adds venue B, which the gateway converts, and C, a FIX 4.3 venue it doesn't:
    /// B takes routes despite C; routes to C are refused and B's kept.
    #[tokio::test]
    async fn a_reload_takes_the_venues_it_can_and_refuses_routes_to_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let listen = "listen = \"127.0.0.1:0\"\n";
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}{listen}"));
        write(&dir, "jetbridge.toml", "sessions = \"sessions.toml\"");
        let path = dir.path().join("jetbridge.toml");
        let (service, serving) = Service::start(&path).await.unwrap();

        write(
            &dir,
            "sessions.toml",
            &format!("{ACCEPTOR}{listen}{}{}", venue_section("B", "FIX.4.4"), venue_section("C", "FIX.4.3")),
        );
        write(&dir, "jetbridge.toml", &routes_to("B"));
        let reloaded = service.reload().unwrap();
        assert_eq!(reloaded.sessions.map(|changes| changes.started), Ok(vec!["B".into(), "C".into()]));
        assert_eq!(reloaded.config.map(|config| config.routes), Ok(1));
        let to_b = Router::new(vec![Rule {
            client: None,
            ex_destination: None,
            symbol: None,
            target: Target::Venue("B".into()),
        }]);
        assert_eq!(service.gateway.router(), to_b);
        assert_eq!(service.set_venues(), ["B"]);

        write(&dir, "jetbridge.toml", &routes_to("C"));
        let reloaded = service.reload().unwrap();
        assert_eq!(reloaded.sessions, Ok(Changes::default()));
        assert!(reloaded.config.unwrap_err().contains("\"C\""));
        assert_eq!(service.gateway.router(), to_b);
        service.shutdown().await;
        serving.abort();
    }

    /// The limits load at start and change with the routes on a reload; a file whose routes are
    /// refused changes neither, though its limits are fine.
    #[tokio::test]
    async fn a_reload_replaces_the_limits_with_the_routes_or_neither() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        let limit = |qty: u32| format!("[risk.defaults]\nmax_order_qty = {qty}\n");
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{}", limit(5)));
        let path = dir.path().join("jetbridge.toml");
        let (service, serving) = Service::start(&path).await.unwrap();
        let max_order_qty = || service.gateway.limits().defaults.max_order_qty;
        assert_eq!(max_order_qty(), Some(5.into()));

        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n{}", limit(7)));
        let config = service.reload().unwrap().config.unwrap();
        assert_eq!((config.routes, config.clients), (0, 0));
        assert_eq!(max_order_qty(), Some(7.into()));
        write(&dir, "jetbridge.toml", &format!("{}{}", routes_to("NOWHERE"), limit(9)));
        service.reload().unwrap();
        assert_eq!(max_order_qty(), Some(7.into()));
        assert_eq!(service.gateway.router(), Router::new(Vec::new()));
        service.shutdown().await;
        serving.abort();
    }

    /// Even with no route to it: a venue the gateway can't speak to is a mistake to fix now, not
    /// at the reload that first routes to it.
    #[tokio::test]
    async fn a_venue_the_gateway_cant_convert_stops_startup() {
        let dir = tempfile::tempdir().unwrap();
        let venues = format!("{}{}", venue_section("B", "FIX.4.4"), venue_section("C", "FIX.4.3"));
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n{venues}"));
        write(&dir, "jetbridge.toml", &routes_to("B"));
        let error = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap_err();
        assert!(error.to_string().contains("venue \"C\": BeginString \"FIX.4.3\" is not supported"), "{error}");
    }

    /// With a database, a client's or venue's session kept in memory stops startup, before the
    /// database is created.
    #[tokio::test]
    async fn a_memory_session_store_with_a_database_stops_startup() {
        let dir = tempfile::tempdir().unwrap();
        let disk = "[store.disk]\nkind = \"disk\"\ndir = \"store\"\n";
        let db = dir.path().join("book.db");
        let config = format!("{}[database]\nurl = \"sqlite://{}?mode=rwc\"\n", routes_to("B"), db.display());
        write(&dir, "jetbridge.toml", &config);
        let start = async |sessions: String| {
            write(&dir, "sessions.toml", &sessions);
            Service::start(&dir.path().join("jetbridge.toml")).await.map(|_| ()).unwrap_err().to_string()
        };
        let venue = venue_section("B", "FIX.4.4");
        let error =
            start(format!("{disk}{ACCEPTOR}listen = \"127.0.0.1:0\"\n[counterparty.C]\n{venue}store = \"disk\"\n"))
                .await;
        assert!(error.contains("the clients' sessions must be kept in a durable store"), "{error}");
        let error = start(format!(
            "{disk}{ACCEPTOR}listen = \"127.0.0.1:0\"\n[defaults]\nstore = \"disk\"\n{venue}store = \"memory\"\n"
        ))
        .await;
        assert!(error.contains("venue B's session must be kept in a durable store"), "{error}");
        assert!(!db.exists());
    }

    /// A reload adding a venue whose session is kept in memory is taken, with a warning:
    /// stopping a running gateway over it would be worse.
    #[tokio::test]
    async fn a_reload_adding_a_memory_session_store_with_a_database_warns() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("book.db");
        let config = format!("{}[database]\nurl = \"sqlite://{}?mode=rwc\"\n", routes_to("B"), db.display());
        write(&dir, "jetbridge.toml", &config);
        let durable = format!(
            "[store.disk]\nkind = \"disk\"\ndir = \"store\"\n{ACCEPTOR}listen = \"127.0.0.1:0\"\n\
             [defaults]\nstore = \"disk\"\n{}store = \"disk\"\n",
            venue_section("B", "FIX.4.4")
        );
        write(&dir, "sessions.toml", &durable);
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        assert!(service.reload().unwrap().warnings.is_empty());
        write(&dir, "sessions.toml", &format!("{durable}{}store = \"memory\"\n", venue_section("C", "FIX.4.4")));
        let reloaded = service.reload().unwrap();
        assert!(reloaded.sessions.is_ok(), "{reloaded:?}");
        let [warning] = reloaded.warnings.as_slice() else { panic!("{reloaded:?}") };
        assert!(warning.contains("venue C's session must be kept in a durable store"), "{warning}");
        service.shutdown().await;
        serving.abort();
    }

    /// With `[acceptor.tls]`, clients log on over TLS: here a client from a sessions file of its
    /// own, trusting the gateway's self-signed certificate.
    #[cfg(feature = "tls")]
    #[tokio::test]
    async fn serves_clients_over_tls() {
        /// Says when the client's logon is answered.
        struct Client(tokio::sync::Notify);
        impl turbojet::Application for Client {
            fn on_logon(&self, _: &SessionHandle) {
                self.0.notify_one();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
        write(&dir, "server.pem", &cert.pem());
        write(&dir, "server.key", &key.serialize_pem());
        let tls = "tls = { cert = \"server.pem\", key = \"server.key\" }\n";
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\nunknown = \"admit\"\n{tls}"));
        write(&dir, "jetbridge.toml", "sessions = \"sessions.toml\"");
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();

        let client = format!(
            "[initiator.JETBRIDGE]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"FUNDA\"\n\
             target_comp_id = \"JETBRIDGE\"\nconnect = [\"{}\"]\n\
             tls = {{ ca = \"server.pem\", server_name = \"localhost\" }}\n",
            service.local_addr()
        );
        write(&dir, "client.toml", &client);
        let client = SessionsFile::load(dir.path().join("client.toml")).unwrap();
        let app = Arc::new(Client(tokio::sync::Notify::new()));
        let initiators = client.initiators(app.clone()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), app.0.notified()).await.expect("no logon over TLS");
        initiators.shutdown(None).await;
        service.shutdown().await;
        serving.abort();
    }

    #[tokio::test]
    async fn a_port_in_use_is_an_error() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"{}\"\n", taken.local_addr().unwrap()));
        write(&dir, "jetbridge.toml", "sessions = \"sessions.toml\"");
        let error = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap_err();
        assert!(error.to_string().starts_with("cannot bind"), "{error}");
    }

    /// The venue's FIX session, telling the test each OrderCancelRequest it gets.
    struct Venue(tokio::sync::mpsc::UnboundedSender<turbojet::Message>);

    impl turbojet::Application for Venue {
        fn on_message(
            &self,
            _: &mut turbojet::Context<'_>,
            msg: &turbojet::Message,
        ) -> Result<(), turbojet::MessageReject> {
            if msg.msg_type() == turbojet::MsgType::OrderCancelRequest {
                let _ = self.0.send(msg.clone());
            }
            Ok(())
        }
    }

    /// An order the venue's disk store holds and the database never had is cancelled once the
    /// venue logs on: the start reads the store before its initiator opens (and locks) it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_cancels_an_order_orphaned_in_a_venues_disk_store() {
        use turbojet::SessionStorage;
        use turbojet::message::tags;
        let dir = tempfile::tempdir().unwrap();
        let (cancels, mut cancelled) = tokio::sync::mpsc::unbounded_channel();
        let venue_store = Arc::new(turbojet::MemoryStorage::new());
        let venue = turbojet::Acceptor::new(
            turbojet::SessionConfig::new("FIX.4.4", "VENUE"),
            venue_store.clone(),
            Arc::new(Venue(cancels)),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(venue.serve(listener));
        let initiator = venue_section("VENUE", "FIX.4.4").replace("127.0.0.1:1", &addr.to_string());
        let stores = "[store.disk]\nkind = \"disk\"\ndir = \"store\"\n";
        write(
            &dir,
            "sessions.toml",
            &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n{stores}{initiator}store = \"disk\"\n"),
        );
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("j.db").display());
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n[database]\nurl = \"{url}\"\n"));

        // What a gateway that stopped had sent: order PREVIO1, never committed.
        let [(_, id, storage)] =
            <[_; 1]>::try_from(SessionsFile::load(dir.path().join("sessions.toml")).unwrap().initiator_stores())
                .map_err(|_| "one initiator")
                .unwrap();
        let order = turbojet::Message::new(turbojet::MsgType::NewOrderSingle)
            .with(tags::BEGIN_STRING, "FIX.4.4")
            .with(tags::MSG_SEQ_NUM, 1_u64)
            .with(tags::SENDER_COMP_ID, id.sender_comp_id.as_str())
            .with(tags::TARGET_COMP_ID, id.target_comp_id.as_str())
            .with(tags::SENDING_TIME, turbojet::fields::UtcTimestamp::now())
            .with(tags::CL_ORD_ID, "PREVIO1")
            .with(tags::SYMBOL, "AAPL")
            .with(tags::SIDE, "1")
            .with(tags::ORDER_QTY, "100");
        let mut log = storage.open(&id).unwrap();
        log.record_outgoing(1, Some(&turbojet::codec::encode(&order).unwrap())).unwrap();
        if let Some(commit) = log.commit().unwrap() {
            commit.run().unwrap();
        }
        drop(log);
        // The venue had it, so expects 2 next.
        let venue_side = SessionId::new("FIX.4.4", id.target_comp_id.as_str(), id.sender_comp_id.as_str());
        venue_store.open(&venue_side).unwrap().set_next_incoming(2).unwrap();

        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        let cancel = tokio::time::timeout(Duration::from_secs(5), cancelled.recv()).await.expect("no cancel").unwrap();
        assert_eq!(cancel.get(tags::ORIG_CL_ORD_ID), Some("PREVIO1"));
        service.shutdown().await;
        serving.abort();
    }

    /// Says when a client's logon is answered.
    struct LoggedOn(tokio::sync::Notify);

    impl turbojet::Application for LoggedOn {
        fn on_logon(&self, _: &SessionHandle) {
            self.0.notify_one();
        }
    }

    /// The sessions and frames of the message log's records in `dir`, so far.
    fn logged(dir: &Path) -> Vec<(Option<String>, Vec<u8>)> {
        let files = turbojet::FileMessageLog::files(dir).unwrap();
        let records = files.iter().flat_map(|file| turbojet::FileMessageLog::read(&file.path).unwrap());
        records
            .filter_map(|record| match record.unwrap() {
                turbojet::LogRecord::Message { session, frame, .. } => Some((session, frame)),
                _ => None,
            })
            .collect()
    }

    /// With `[message_log]`, the clients' and the venues' sessions log to its one directory, a
    /// client's Password(554) masked; a reload changing it warns and keeps the log in use.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_message_log_has_both_sides_and_a_reload_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let (cancels, _) = tokio::sync::mpsc::unbounded_channel();
        let venue = turbojet::Acceptor::new(
            turbojet::SessionConfig::new("FIX.4.4", "VENUE"),
            Arc::new(turbojet::MemoryStorage::new()),
            Arc::new(Venue(cancels)),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let venue_addr = listener.local_addr().unwrap();
        tokio::spawn(venue.serve(listener));
        let initiator = venue_section("VENUE", "FIX.4.4").replace("127.0.0.1:1", &venue_addr.to_string());
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\nunknown = \"admit\"\n{initiator}"));
        let jetbridge = "sessions = \"sessions.toml\"\n[message_log]\ndir = \"log\"\n";
        write(&dir, "jetbridge.toml", jetbridge);
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();

        let mut config = turbojet::InitiatorConfig::new(turbojet::SessionConfig::new("FIX.4.4", "FUNDA"), "JETBRIDGE");
        config.password = Some("hunter2".to_string().into());
        let app = Arc::new(LoggedOn(tokio::sync::Notify::new()));
        let addr = service.local_addr().to_string();
        let client =
            turbojet::Initiator::new(addr, config, Arc::new(turbojet::MemoryStorage::new()), app.clone()).unwrap();
        tokio::spawn(client.clone().run());
        tokio::time::timeout(Duration::from_secs(5), app.0.notified()).await.expect("no logon");
        // The log's writer thread gathers records for a millisecond before writing them.
        let has = |records: &[(Option<String>, Vec<u8>)], comp_id: &str| {
            records.iter().any(|(session, _)| session.as_ref().is_some_and(|s| s.contains(comp_id)))
        };
        let records = async {
            loop {
                let records = logged(&dir.path().join("log"));
                if has(&records, "FUNDA") && has(&records, "VENUE") {
                    return records;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let records = tokio::time::timeout(Duration::from_secs(5), records).await.expect("the log lacks a side");
        let frames = records.iter().map(|(_, frame)| String::from_utf8_lossy(frame)).collect::<Vec<_>>();
        assert!(frames.iter().any(|frame| frame.contains("\x01554=*")), "{frames:?}");
        assert!(!frames.iter().any(|frame| frame.contains("hunter2")), "{frames:?}");

        write(&dir, "jetbridge.toml", &format!("{jetbridge}retention = \"30d\"\n"));
        let reloaded = tokio::task::spawn_blocking({
            let service = service.clone();
            move || service.reload().unwrap()
        })
        .await
        .unwrap();
        assert!(reloaded.config.is_ok(), "{reloaded:?}");
        assert!(reloaded.warnings.iter().any(|w| w.contains("[message_log] changed")), "{reloaded:?}");
        assert_eq!(service.message_log.as_ref().unwrap().retention, None);
        client.shutdown(None).await;
        service.shutdown().await;
        serving.abort();
    }

    /// Without `[message_log]`, nothing is logged; a reload adding one warns, and still logs nothing.
    /// Adding a `[database]` warns too.
    #[tokio::test]
    async fn no_message_log_table_no_log() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        write(&dir, "jetbridge.toml", "sessions = \"sessions.toml\"\n");
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        assert!(service.message_log.is_none());
        let changed =
            "sessions = \"sessions.toml\"\n[message_log]\ndir = \"log\"\n[database]\nurl = \"sqlite://j.db\"\n";
        write(&dir, "jetbridge.toml", changed);
        let reloaded = service.reload().unwrap();
        assert!(reloaded.warnings.iter().any(|w| w.contains("[message_log] changed")), "{reloaded:?}");
        assert!(reloaded.warnings.iter().any(|w| w.contains("[database] url changed")), "{reloaded:?}");
        assert!(!dir.path().join("log").exists());
        service.shutdown().await;
        serving.abort();
    }

    /// Two starts one after the other take prefixes from different seconds, however quick.
    #[tokio::test]
    async fn back_to_back_starts_take_different_prefixes() {
        let first = next_second().await;
        assert_ne!(book::start_prefix(first), book::start_prefix(next_second().await));
    }

    /// With a database, the pruning task runs, by the `[database] retention` in force: a reload
    /// changes it without a warning, one under a day is refused, and taking it out stops pruning.
    #[tokio::test]
    async fn a_reload_changes_the_retention_in_force() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("j.db").display());
        let config = |more: &str| format!("sessions = \"sessions.toml\"\n[database]\nurl = \"{url}\"\n{more}");
        write(&dir, "jetbridge.toml", &config("retention = \"400d\"\n"));
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        let retention = || *service.retention.lock().unwrap();
        assert!(service.pruning.is_some());
        assert_eq!(retention(), Some(Duration::from_secs(400 * 86_400)));
        write(&dir, "jetbridge.toml", &config("retention = \"2d\"\n"));
        assert!(service.reload().unwrap().warnings.is_empty());
        assert_eq!(retention(), Some(Duration::from_secs(2 * 86_400)));
        write(&dir, "jetbridge.toml", &config("retention = \"1h\"\n"));
        assert!(service.reload().unwrap().config.is_err());
        assert_eq!(retention(), Some(Duration::from_secs(2 * 86_400)));
        write(&dir, "jetbridge.toml", &config(""));
        assert!(service.reload().unwrap().config.is_ok());
        assert_eq!(retention(), None);
        service.shutdown().await;
        serving.abort();
    }

    /// The first prune is an hour after the start, not at it: each run reads the gateway's clock,
    /// here counting, before its first await. Later runs aren't checked: the paused clock races
    /// ahead of the database, whose run delays the next tick.
    #[tokio::test]
    async fn the_first_prune_is_an_hour_after_the_start() {
        use std::sync::atomic::AtomicUsize;
        let store = Store::open("sqlite::memory:").await.unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let clock = {
            let runs = runs.clone();
            turbojet::Clock::from_fn(move || {
                runs.fetch_add(1, Ordering::SeqCst);
                chrono::DateTime::UNIX_EPOCH
            })
        };
        let gateway = Arc::new(Gateway::new(Router::new(Vec::new()), Version::Fix44).with_clock(clock));
        tokio::time::pause();
        let retention = Arc::new(Mutex::new(Some(Duration::from_secs(86_400))));
        let pruning = tokio::spawn(prune_hourly(gateway, store, retention));
        let runs_after = async |wait: Duration| {
            tokio::time::sleep(wait).await;
            runs.load(Ordering::SeqCst)
        };
        assert_eq!(runs_after(Duration::ZERO).await, 0);
        assert_eq!(runs_after(PRUNE_INTERVAL - Duration::from_millis(1)).await, 0);
        assert_eq!(runs_after(Duration::from_millis(2)).await, 1);
        pruning.abort();
    }

    /// A database with rows in it is restored from at the next start: here a block.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_restores_the_book_from_the_database() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n"));
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("j.db").display());
        write(&dir, "jetbridge.toml", &format!("sessions = \"sessions.toml\"\n[database]\nurl = \"{url}\"\n"));
        let path = dir.path().join("jetbridge.toml");
        let (service, serving) = Service::start(&path).await.unwrap();
        service.gateway().block(book::Scope::All).await;
        service.shutdown().await;
        serving.abort();
        drop(service);

        let (service, serving) = Service::start(&path).await.unwrap();
        assert_eq!(service.gateway().blocked(), [book::Scope::All]);
        service.shutdown().await;
        serving.abort();
    }
}
