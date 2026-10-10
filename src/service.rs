//! The gateway running from its configuration files: the acceptor serving clients, an initiator
//! per venue, and reloading. The binary adds arguments and signals.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use turbojet::{Acceptor, SessionHandle, SessionId};
use turbojet_config::{Initiators, SessionsFile};

use crate::book;
use crate::config::{self, Error};
use crate::edge::{CANCEL_RETRY_INTERVAL, ClientApp, Gateway, VenueApp};
use crate::fix::Version;

/// The Text(58) of the Logouts sent at shutdown.
const SHUTTING_DOWN: &str = "gateway shutting down";

/// How long shutdown waits, with the clients gone, for the venues to answer the gateway's
/// cancels (those cancel on disconnect sends as the clients log out, say). An order left working
/// at a venue with no gateway to report its fills is worse than a slower stop; a venue that
/// doesn't answer in this long isn't going to soon.
const CANCELS_WAIT: Duration = Duration::from_secs(5);

/// The gateway, serving.
#[derive(Debug)]
pub struct Service {
    path: PathBuf,
    gateway: Arc<Gateway>,
    sessions: SessionsFile,
    acceptor: Acceptor,
    initiators: Initiators,
    local_addr: SocketAddr,
    /// [`Gateway::retry_cancels`], aborted at shutdown, or when the service is dropped.
    retrying: JoinHandle<()>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.retrying.abort();
    }
}

/// The acceptor's serving task; it ends only if accepting fails.
pub type Serving = JoinHandle<io::Result<()>>;

impl Service {
    /// Loads the configuration file at `path`, starts the venues' initiators, and serves the
    /// clients' acceptor. Call it within a tokio runtime.
    ///
    /// # Errors
    ///
    /// The configuration doesn't load, the acceptor's address can't be bound, a venue speaks a FIX
    /// version the gateway doesn't convert, or TLS or a venue can't be set up.
    pub async fn start(path: &Path) -> Result<(Self, Serving), Error> {
        let loaded = config::load(path)?;
        let prefix = book::start_prefix(SystemTime::now());
        let gateway =
            Arc::new(Gateway::new(loaded.router, loaded.version).with_id_prefix(prefix).with_limits(loaded.limits));
        let sessions = loaded.sessions;
        let listen = sessions.listen().expect("load checked the file has [acceptor]");
        // Bound before the venues start, so a port in use stops nothing half-started.
        let listener =
            TcpListener::bind(&listen).await.map_err(|e| Error::new(format!("cannot bind {listen}: {e}")))?;
        let local_addr = listener.local_addr().map_err(|e| Error::new(format!("{listen}: {e}")))?;
        let initiators = sessions.initiators(Arc::new(VenueApp(gateway.clone()))).map_err(Error::new)?;
        let acceptor = sessions.acceptor(Arc::new(ClientApp(gateway.clone()))).expect("load checked [acceptor]");
        let retrying = tokio::spawn(gateway.clone().retry_cancels(CANCEL_RETRY_INTERVAL));
        let service = Self { path: path.to_path_buf(), gateway, sessions, acceptor, initiators, local_addr, retrying };
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
        Ok((service, serving))
    }

    /// The address the acceptor listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The gateway: the book and where its outputs go.
    pub fn gateway(&self) -> &Arc<Gateway> {
        &self.gateway
    }

    /// The client session `id`, connected or not.
    pub fn client_session(&self, id: &SessionId) -> SessionHandle {
        self.acceptor.handle(id)
    }

    /// The session of venue `name` (its `[initiator.NAME]`), if its initiator is running.
    pub fn venue_session(&self, name: &str) -> Option<SessionHandle> {
        self.initiators.handle(name)
    }

    /// Rereads the sessions file, and the routes and limits, logging what changed. Either file that
    /// doesn't load is logged and stays as it was (routes and limits together); the routes are
    /// checked against the venues the gateway took after the sessions reload, so a venue added to
    /// both files at once can be routed to.
    pub fn reload(&self) {
        match self.sessions.reload_all(Some(&self.acceptor), &self.initiators) {
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
        let venues = self.set_venues();
        match config::reload(&self.path, &venues) {
            Ok((router, limits)) => {
                info!(
                    routes = router.rules().len(),
                    clients = limits.by_comp_id.len(),
                    "reloaded the routes and limits"
                );
                self.gateway.set_config(router, limits);
            }
            Err(e) => warn!("keeping the routes and limits in use: {e}"),
        }
    }

    /// Logs the clients' sessions out, cancels the working orders of those with cancel on
    /// disconnect (pending countdowns fire as the clients go; the shutdown's own Logouts start
    /// none), waits for the venues to answer the gateway's cancels, logs the venues' sessions out,
    /// returning once all have closed, then logs what the stop loses.
    pub async fn shutdown(&self) {
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
        // Lost at exit until stage 4 persists the book and the undelivered reports: open orders
        // stay open at their venues with no client to report to.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::{Router, Rule, Target};

    const ACCEPTOR: &str = "[acceptor]\nbegin_string = \"FIX.4.4\"\nsender_comp_id = \"JETBRIDGE\"\n";

    /// An initiator section for venue `name`, speaking `begin_string`, retrying a closed port.
    fn venue(name: &str, begin_string: &str) -> String {
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
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/examples/jetbridge.toml"),
            dir.path().join("jetbridge.toml"),
        )
        .unwrap();
        let (service, serving) = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap();
        assert_ne!(service.local_addr().port(), 0);
        assert!(tokio::net::TcpStream::connect(service.local_addr()).await.is_ok());
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

        write(&dir, "sessions.toml", &format!("{ACCEPTOR}{listen}{}{}", venue("B", "FIX.4.4"), venue("C", "FIX.4.3")));
        write(&dir, "jetbridge.toml", &routes_to("B"));
        service.reload();
        let to_b = Router::new(vec![Rule {
            client: None,
            ex_destination: None,
            symbol: None,
            target: Target::Venue("B".into()),
        }]);
        assert_eq!(service.gateway.router(), to_b);
        assert_eq!(service.set_venues(), ["B"]);

        write(&dir, "jetbridge.toml", &routes_to("C"));
        service.reload();
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
        service.reload();
        assert_eq!(max_order_qty(), Some(7.into()));
        write(&dir, "jetbridge.toml", &format!("{}{}", routes_to("NOWHERE"), limit(9)));
        service.reload();
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
        let venues = format!("{}{}", venue("B", "FIX.4.4"), venue("C", "FIX.4.3"));
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n{venues}"));
        write(&dir, "jetbridge.toml", &routes_to("B"));
        let error = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap_err();
        assert!(error.to_string().contains("venue \"C\": BeginString \"FIX.4.3\" is not supported"), "{error}");
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
}
