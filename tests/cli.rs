//! The binary's `user` and `token` commands, run as an operator would, against a users file.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use jetbridge::auth::{Users, verify_password};

/// Runs `jetbridge args`, `stdin` piped in.
fn jetbridge(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jetbridge"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn users(path: &Path) -> Users {
    Users::load(path).unwrap()
}

/// Adds a user (the password twice from stdin), gives them a token, checks both authenticate,
/// revokes the token by its first characters, and removes the user.
#[test]
fn the_users_commands_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("users.toml");
    let file = path.to_str().unwrap();

    let added =
        jetbridge(&["user", "add", "alice", "--role", "operator", "--clients", "FUNDA", "--users", file], "pw1\npw1\n");
    assert!(added.status.success(), "{added:?}");
    let alice = users(&path).get("alice").cloned().unwrap();
    assert!(verify_password(&alice.password, "pw1"));
    assert_eq!(alice.clients, ["FUNDA"]);

    let differ = jetbridge(&["user", "add", "bob", "--role", "admin", "--users", file], "a\nb\n");
    assert!(!differ.status.success());
    assert!(String::from_utf8_lossy(&differ.stderr).contains("the passwords differ"));
    assert!(users(&path).get("bob").is_none());

    let added = jetbridge(&["token", "add", "alice", "--users", file], "");
    assert!(added.status.success(), "{added:?}");
    let token = String::from_utf8(added.stdout).unwrap().trim().to_string();
    assert_eq!(users(&path).by_token(&token).map(|(name, _)| name), Some("alice"));
    assert!(!std::fs::read_to_string(&path).unwrap().contains(&token), "the token itself is kept");

    let revoked = jetbridge(&["token", "revoke", "alice", &token[..6], "--users", file], "");
    assert!(revoked.status.success(), "{revoked:?}");
    assert_eq!(users(&path).by_token(&token), None);

    assert!(jetbridge(&["user", "remove", "alice", "--users", file], "").status.success());
    assert!(users(&path).get("alice").is_none());
    assert!(!jetbridge(&["user", "remove", "alice", "--users", file], "").status.success());
}
