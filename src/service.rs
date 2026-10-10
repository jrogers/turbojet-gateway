//! The gateway running from its configuration files: the acceptor serving clients, an initiator
//! per venue, and reloading. The binary adds arguments and signals.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use turbojet::{Acceptor, SessionHandle};
use turbojet_config::{Initiators, SessionsFile};

use crate::config::{self, Error};
use crate::edge::{ClientApp, Gateway, VenueApp};

/// The Text(58) of the Logouts sent at shutdown.
const SHUTTING_DOWN: &str = "gateway shutting down";

/// The gateway, serving.
#[derive(Debug)]
pub struct Service {
    path: PathBuf,
    gateway: Arc<Gateway>,
    sessions: SessionsFile,
    acceptor: Acceptor,
    initiators: Initiators,
    local_addr: SocketAddr,
}

/// The acceptor's serving task; it ends only if accepting fails.
pub type Serving = JoinHandle<io::Result<()>>;

impl Service {
    /// Loads the configuration file at `path`, starts the venues' initiators, and serves the
    /// clients' acceptor. Call it within a tokio runtime.
    ///
    /// # Errors
    ///
    /// The configuration doesn't load, the acceptor's address can't be bound, or TLS or a venue
    /// can't be set up.
    pub async fn start(path: &Path) -> Result<(Self, Serving), Error> {
        let loaded = config::load(path)?;
        let gateway = Arc::new(Gateway::new(loaded.router, loaded.version));
        let sessions = loaded.sessions;
        let listen = sessions.listen().expect("load checked the file has [acceptor]");
        // Bound before the venues start, so a port in use stops nothing half-started.
        let listener =
            TcpListener::bind(&listen).await.map_err(|e| Error::new(format!("cannot bind {listen}: {e}")))?;
        let local_addr = listener.local_addr().map_err(|e| Error::new(format!("{listen}: {e}")))?;
        let initiators = sessions.initiators(Arc::new(VenueApp(gateway.clone()))).map_err(Error::new)?;
        let acceptor = sessions.acceptor(Arc::new(ClientApp(gateway.clone()))).expect("load checked [acceptor]");
        let service = Self { path: path.to_path_buf(), gateway, sessions, acceptor, initiators, local_addr };
        config::check_venues(path, &service.gateway.router(), &service.set_venues())?;
        let serving = service.serve(listener)?;
        info!(addr = %local_addr, version = ?loaded.version, venues = ?service.initiators.names(), "gateway listening");
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

    /// The session of the client whose CompID is `comp_id`, connected or not.
    pub fn client_session(&self, comp_id: &str) -> SessionHandle {
        self.acceptor.session(comp_id)
    }

    /// The session of venue `name` (its `[initiator.NAME]`), if its initiator is running.
    pub fn venue_session(&self, name: &str) -> Option<SessionHandle> {
        self.initiators.handle(name)
    }

    /// Rereads the sessions file and the routes, logging what changed. Either that doesn't load
    /// is logged and stays as it was; the routes are checked against the venues the gateway took
    /// after the sessions reload, so a venue added to both files at once can be routed to.
    pub fn reload(&self) {
        match self.sessions.reload_all(Some(&self.acceptor), &self.initiators) {
            Ok(changes) => info!(
                added = ?changes.added,
                changed = ?changes.changed,
                removed = ?changes.removed,
                logged_out = ?changes.logged_out,
                started = ?changes.started,
                reconfigured = ?changes.reconfigured,
                stopped = ?changes.stopped,
                "reloaded the sessions file"
            ),
            Err(e) => warn!("keeping the sessions file in use: {e}"),
        }
        let venues = self.set_venues();
        match config::reload_routes(&self.path, &venues) {
            Ok(router) => {
                info!(routes = router.rules().len(), "reloaded the routes");
                self.gateway.set_router(router);
            }
            Err(e) => warn!("keeping the routes in use: {e}"),
        }
    }

    /// Logs the venues' and clients' sessions out, returning once all have closed, then logs
    /// what the stop loses.
    pub async fn shutdown(&self) {
        tokio::join!(self.initiators.shutdown(Some(SHUTTING_DOWN)), self.acceptor.shutdown(Some(SHUTTING_DOWN)));
        // Lost at exit until stage 4 persists the book and the undelivered reports: open orders
        // stay open at their venues with no client to report to.
        let (open_orders, undelivered) = self.gateway.unfinished();
        if open_orders.is_empty() && undelivered.is_empty() {
            info!("no open orders or undelivered reports");
        } else {
            warn!(
                ?open_orders,
                ?undelivered,
                "stopping with open orders (per venue) and undelivered reports (per client)"
            );
        }
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

    #[tokio::test]
    async fn a_route_to_a_venue_the_gateway_cant_convert_stops_startup() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "sessions.toml", &format!("{ACCEPTOR}listen = \"127.0.0.1:0\"\n{}", venue("C", "FIX.4.3")));
        write(&dir, "jetbridge.toml", &routes_to("C"));
        let error = Service::start(&dir.path().join("jetbridge.toml")).await.unwrap_err();
        assert!(error.to_string().contains("route 1: venue \"C\""), "{error}");
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
