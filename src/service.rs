//! The gateway running from its configuration files: the acceptor serving clients, an initiator
//! per venue, and reloading. The binary adds arguments and signals.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use turbojet::Acceptor;
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
        service.set_venues()?;
        let serving = service.serve(listener)?;
        info!(addr = %local_addr, version = ?loaded.version, venues = ?service.initiators.names(), "gateway listening");
        Ok((service, serving))
    }

    /// The address the acceptor listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Rereads the sessions file and the routes, logging what changed. Either that doesn't load
    /// is logged and stays as it was; the routes are checked against the venues after the
    /// sessions reload, so a venue added to both files at once can be routed to.
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
        if let Err(e) = self.set_venues() {
            error!("keeping the venues in use: {e}");
        }
        match config::reload_routes(&self.path, &self.sessions) {
            Ok(router) => {
                info!(routes = router.rules().len(), "reloaded the routes");
                self.gateway.set_router(router);
            }
            Err(e) => warn!("keeping the routes in use: {e}"),
        }
    }

    /// Logs the venues' and clients' sessions out, returning once all have closed.
    pub async fn shutdown(&self) {
        tokio::join!(self.initiators.shutdown(Some(SHUTTING_DOWN)), self.acceptor.shutdown(Some(SHUTTING_DOWN)));
    }

    /// Tells the gateway the venues running now: a reload may have started or stopped some.
    fn set_venues(&self) -> Result<(), Error> {
        let names = self.initiators.names();
        let venues = names.into_iter().filter_map(|name| Some((name.as_str().into(), self.initiators.handle(&name)?)));
        self.gateway.set_venues(venues).map_err(Error::new)
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

    /// Starts from files on an ephemeral port, with the venue's initiator retrying a closed port,
    /// and shuts down. Orders flowing end to end are `tests/e2e.rs`'s.
    #[tokio::test]
    async fn starts_from_its_files_and_shuts_down() {
        let dir = std::env::temp_dir().join(format!("jetbridge-service-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/sessions.toml"))
            .unwrap()
            .replace("127.0.0.1:9876", "127.0.0.1:0")
            .replace("127.0.0.1:9877", "127.0.0.1:1");
        std::fs::write(dir.join("sessions.toml"), sessions).unwrap();
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/jetbridge.toml"), dir.join("jetbridge.toml"))
            .unwrap();
        let (service, serving) = Service::start(&dir.join("jetbridge.toml")).await.unwrap();
        assert_ne!(service.local_addr().port(), 0);
        assert!(tokio::net::TcpStream::connect(service.local_addr()).await.is_ok());
        service.reload();
        service.shutdown().await;
        serving.abort();
    }

    #[tokio::test]
    async fn a_port_in_use_is_an_error() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dir = std::env::temp_dir().join(format!("jetbridge-service-taken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = "[acceptor]\nbegin_string = \"FIX.4.2\"\nsender_comp_id = \"JETBRIDGE\"\n";
        let sessions = format!("{sessions}listen = \"{}\"\n", taken.local_addr().unwrap());
        std::fs::write(dir.join("sessions.toml"), sessions).unwrap();
        std::fs::write(dir.join("jetbridge.toml"), "sessions = \"sessions.toml\"").unwrap();
        let error = Service::start(&dir.join("jetbridge.toml")).await.unwrap_err();
        assert!(error.to_string().starts_with("cannot bind"), "{error}");
    }
}
