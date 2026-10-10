//! The Jetbridge binary; the gateway itself is the `jetbridge` library.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use jetbridge::service::Service;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
usage: jetbridge --config FILE

A FIX gateway: clients log on to its acceptor, and it routes their orders to venues.

  --config FILE   jetbridge.toml: the sessions file and the routes
  --help          print this

On Unix, SIGHUP reloads both files; a file that doesn't load keeps the one in use. Ctrl-C or
SIGTERM logs every session out and exits. RUST_LOG sets what's logged (info by default).";

#[tokio::main]
async fn main() -> ExitCode {
    let path = match parse_args(std::env::args().skip(1)) {
        Ok(Some(path)) => path,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let (service, serving) = match Service::start(&path).await {
        Ok(started) => started,
        Err(e) => {
            error!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let service = Arc::new(service);
    let reloading = reload_on_hangup(service.clone());
    let code = tokio::select! {
        result = serving => {
            match result {
                Ok(Err(e)) => error!("accepting failed: {e}"),
                Ok(Ok(())) => error!("accepting stopped"),
                Err(e) => error!("accepting failed: {e}"),
            }
            ExitCode::FAILURE
        }
        () = shutdown_signal() => ExitCode::SUCCESS,
    };
    // A reload while shutting down would start initiators being stopped.
    reloading.abort();
    info!("shutting down: logging sessions out (signal again to exit at once)");
    tokio::select! {
        () = service.shutdown() => info!("all sessions closed"),
        () = shutdown_signal() => warn!("exiting without waiting for sessions to log out"),
    }
    code
}

/// The configuration file's path, or `None` for `--help`.
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<PathBuf>, String> {
    let mut config = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--config" => config = Some(args.next().ok_or("--config needs a file")?.into()),
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    config.map(Some).ok_or_else(|| "--config is required".to_string())
}

/// On Unix, reloads the configuration on each SIGHUP, until the task is aborted.
fn reload_on_hangup(service: Arc<Service>) -> tokio::task::JoinHandle<()> {
    #[cfg(unix)]
    return tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut hangup) = signal(SignalKind::hangup()) else {
            warn!("cannot listen for SIGHUP; the configuration won't be reloaded");
            return;
        };
        while hangup.recv().await.is_some() {
            service.reload();
        }
    });
    #[cfg(not(unix))]
    {
        let _ = service;
        tokio::spawn(async {})
    }
}

/// Completes on Ctrl-C (SIGINT) or, on Unix, SIGTERM, which service managers and `docker stop`
/// send.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            warn!("cannot listen for SIGTERM; only Ctrl-C shuts down");
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<PathBuf>, String> {
        parse_args(args.iter().map(ToString::to_string))
    }

    #[test]
    fn parses_the_config_path_and_help() {
        assert_eq!(parse(&["--config", "j.toml"]), Ok(Some("j.toml".into())));
        assert_eq!(parse(&["--help"]), Ok(None));
        assert_eq!(parse(&[]), Err("--config is required".into()));
        assert_eq!(parse(&["--config"]), Err("--config needs a file".into()));
        assert_eq!(parse(&["-x"]), Err("unknown argument \"-x\"".into()));
    }
}
