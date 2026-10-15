//! The Turbojet Hub binary; the gateway itself is the `turbojet_hub` library.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use turbojet_hub::auth::{Role, Users};
use turbojet_hub::service::Service;

const USAGE: &str = "\
usage: turbojet-hub --config FILE
       turbojet-hub user add NAME --role viewer|operator|admin [--clients A,B] --users FILE
       turbojet-hub user passwd NAME --users FILE
       turbojet-hub user remove NAME --users FILE
       turbojet-hub token add NAME --users FILE
       turbojet-hub token revoke NAME PREFIX --users FILE

A FIX gateway: clients log on to its acceptor, and it routes their orders to venues.

  --config FILE   turbojet-hub.toml: the sessions file, the routes and the risk limits
  --help          print this

On Unix, SIGHUP reloads the files; a file that doesn't load keeps the one in use. Ctrl-C or
SIGTERM logs every session out and exits. RUST_LOG sets what's logged (info by default).

The user and token commands edit the operator API's users file, creating it if need be; a
reload applies the change. Run them as the gateway's user: the rewritten file belongs to whoever
runs them. `user add` and `user passwd` ask for the password twice (one line each from stdin
when it isn't a terminal), at least 12 bytes; --clients limits the user to acting on those
CompIDs. `user passwd` keeps the user's role, clients and tokens, and ends their sessions.
`token add` prints the new token once: only its hash is kept. `token revoke` takes the token, or
the id `token add` showed, or its first characters.";

/// What the arguments ask for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(PathBuf),
    Help,
    /// Edits the users file `users`.
    Users {
        users: PathBuf,
        change: Change,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    AddUser { name: String, role: Role, clients: Vec<String> },
    SetPassword { name: String },
    RemoveUser { name: String },
    AddToken { name: String },
    RevokeToken { name: String, prefix: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let path = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Run(path)) => path,
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Users { users, change }) => {
            return match edit_users(&users, change) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
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

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut config = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--config" => config = Some(args.next().ok_or("--config needs a file")?.into()),
            "user" | "token" if config.is_none() => return parse_users_args(&arg, args),
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    config.map(Command::Run).ok_or_else(|| "--config is required".to_string())
}

/// The arguments after `user` or `token` (`kind`).
fn parse_users_args(kind: &str, mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let (mut words, mut users, mut role, mut clients) = (Vec::new(), None, None, None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--users" => users = Some(PathBuf::from(value()?)),
            "--role" => role = Some(value()?.parse::<Role>()?),
            "--clients" => clients = Some(value()?.split(',').map(|c| c.trim().to_string()).collect::<Vec<_>>()),
            _ if arg.starts_with('-') => return Err(format!("unknown argument {arg:?}")),
            _ => words.push(arg),
        }
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let change = match (kind, words.as_slice()) {
        ("user", ["add", name]) => Change::AddUser {
            name: (*name).to_string(),
            role: role.take().ok_or("user add needs --role")?,
            clients: clients.take().unwrap_or_default(),
        },
        ("user", ["passwd", name]) => Change::SetPassword { name: (*name).to_string() },
        ("user", ["remove", name]) => Change::RemoveUser { name: (*name).to_string() },
        ("token", ["add", name]) => Change::AddToken { name: (*name).to_string() },
        ("token", ["revoke", name, prefix]) => {
            Change::RevokeToken { name: (*name).to_string(), prefix: (*prefix).to_string() }
        }
        _ => return Err(format!("unknown command: {kind} {}", words.join(" "))),
    };
    if role.is_some() || clients.is_some() {
        return Err("--role and --clients are for user add".to_string());
    }
    let users = users.ok_or("--users is required")?;
    Ok(Command::Users { users, change })
}

/// Makes `change` to the users file at `path`, rewriting it.
fn edit_users(path: &Path, change: Change) -> Result<(), String> {
    let mut users = Users::load_or_new(path)?;
    match change {
        Change::AddUser { name, role, clients } => {
            if users.get(&name).is_some() {
                return Err(format!("user {name:?} exists: change its password with user passwd"));
            }
            users.add(&name, role, clients, &new_password(&name)?)?;
            eprintln!("added {role} {name}");
        }
        Change::SetPassword { name } => {
            if users.get(&name).is_none() {
                return Err(format!("no user {name:?}"));
            }
            users.set_password(&name, &new_password(&name)?)?;
            eprintln!("changed {name}'s password");
        }
        Change::RemoveUser { name } => {
            if !users.remove(&name) {
                return Err(format!("no user {name:?}"));
            }
            eprintln!("removed {name}");
        }
        Change::AddToken { name } => {
            let token = users.add_token(&name)?;
            // The token alone on stdout, for scripts; it's shown this once.
            println!("{token}");
            eprintln!(
                "added a token for {name}, id {}: keep it, it isn't shown again",
                turbojet_hub::auth::token_id(&token)
            );
        }
        Change::RevokeToken { name, prefix } => {
            let id = users.revoke_token(&name, &prefix)?;
            eprintln!("revoked {name}'s token {id}");
        }
    }
    if let Some(warning) = users.save(path).map_err(|e| format!("cannot write {}: {e}", path.display()))? {
        eprintln!("warning: {warning}");
    }
    eprintln!("reload the gateway (SIGHUP) for it to take effect");
    Ok(())
}

/// A new password for `name`, asked for twice.
fn new_password(name: &str) -> Result<String, String> {
    let password = read_password(&format!("password for {name}: "))?;
    if read_password("again: ")? != password {
        return Err("the passwords differ".to_string());
    }
    turbojet_hub::auth::check_new_password(&password)?;
    Ok(password)
}

/// A password: typed without echo at a terminal, else a line from stdin.
fn read_password(prompt: &str) -> Result<String, String> {
    let password = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt)
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line.trim_end_matches(['\r', '\n']).to_string())
    };
    password.map_err(|e| format!("cannot read the password: {e}"))
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
            // Off the async workers: it reads files, and waits for a reload the API started.
            let service = service.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || service.reload()).await {
                warn!("the reload failed: {e}");
            }
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

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(ToString::to_string))
    }

    #[test]
    fn parses_the_config_path_and_help() {
        assert_eq!(parse(&["--config", "j.toml"]), Ok(Command::Run("j.toml".into())));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&[]), Err("--config is required".into()));
        assert_eq!(parse(&["--config"]), Err("--config needs a file".into()));
        assert_eq!(parse(&["-x"]), Err("unknown argument \"-x\"".into()));
    }

    #[test]
    fn parses_the_users_commands() {
        let users = |change| Ok(Command::Users { users: "u.toml".into(), change });
        assert_eq!(
            parse(&["user", "add", "alice", "--role", "operator", "--clients", "A, B", "--users", "u.toml"]),
            users(Change::AddUser {
                name: "alice".into(),
                role: Role::Operator,
                clients: vec!["A".into(), "B".into()]
            })
        );
        assert_eq!(
            parse(&["user", "add", "--users", "u.toml", "bob", "--role", "admin"]),
            users(Change::AddUser { name: "bob".into(), role: Role::Admin, clients: Vec::new() })
        );
        assert_eq!(
            parse(&["user", "remove", "bob", "--users", "u.toml"]),
            users(Change::RemoveUser { name: "bob".into() })
        );
        assert_eq!(
            parse(&["user", "passwd", "bob", "--users", "u.toml"]),
            users(Change::SetPassword { name: "bob".into() })
        );
        assert_eq!(
            parse(&["token", "add", "bob", "--users", "u.toml"]),
            users(Change::AddToken { name: "bob".into() })
        );
        assert_eq!(
            parse(&["token", "revoke", "bob", "q3Xr", "--users", "u.toml"]),
            users(Change::RevokeToken { name: "bob".into(), prefix: "q3Xr".into() })
        );
        assert_eq!(parse(&["user", "add", "bob", "--users", "u.toml"]), Err("user add needs --role".into()));
        assert_eq!(
            parse(&["user", "add", "bob", "--role", "root", "--users", "u"]).unwrap_err(),
            "unknown role \"root\": viewer, operator or admin"
        );
        assert_eq!(
            parse(&["user", "remove", "bob", "--role", "admin", "--users", "u"]),
            Err("--role and --clients are for user add".into())
        );
        assert_eq!(parse(&["token", "add", "bob"]), Err("--users is required".into()));
        assert_eq!(parse(&["token", "drop", "bob", "--users", "u"]), Err("unknown command: token drop bob".into()));
        assert_eq!(parse(&["user", "add", "bob", "--role"]), Err("--role needs a value".into()));
    }
}
