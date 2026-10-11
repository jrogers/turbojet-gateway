//! The operator API's users: `users.toml`, their roles, passwords and tokens.
//!
//! ```toml
//! [user.alice]
//! role = "operator"                 # viewer, operator or admin; each may do what those before it may
//! password = "$argon2id$v=19$..."   # a PHC string: `jetbridge user add` writes it
//! clients = ["FUNDA"]               # optional: acts on these CompIDs only; all, if left out
//!
//! [[user.alice.tokens]]             # `jetbridge token add` writes these
//! id = "q3Xr9a_T"                   # the token's first characters after `jb_`, to revoke it by
//! sha256 = "5f2c..."                # the token's SHA-256, hex: the token itself isn't kept
//! ```
//!
//! An empty file (or one with only comments) has no users. Unknown keys are errors. The binary's
//! `user` and `token` commands edit the file, rewriting it whole (comments aren't kept); the
//! gateway rereads it at start and on reload.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{self, Write as _};
use std::path::Path;
use std::str::FromStr;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64ct::{Base64UrlUnpadded, Encoding};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// A user name's length at most, in bytes. Login names longer than any user's are refused
/// before the login limit counts them, so this also bounds what the limit keeps per name.
pub const MAX_NAME_LEN: usize = 64;

/// What a token starts with: tells it from a password in a script, or in a leak scan.
const TOKEN_PREFIX: &str = "jb_";

/// A token's random bytes: as many as a SHA-256, so guessing one is hopeless.
const TOKEN_BYTES: usize = 32;

/// How many characters of a token (after [`TOKEN_PREFIX`]) its id keeps: enough to tell a user's
/// tokens apart, and few enough of its 43 that the rest stays secret.
const TOKEN_ID_LEN: usize = 8;

/// What a user may do: each role may do what those before it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Reads.
    Viewer,
    /// Reads, pauses and resumes sessions, cancels orders and blocks clients.
    Operator,
    /// All of it, reloading the configuration and reading the audit log too.
    Admin,
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "viewer" => Ok(Self::Viewer),
            "operator" => Ok(Self::Operator),
            "admin" => Ok(Self::Admin),
            _ => Err(format!("unknown role {s:?}: viewer, operator or admin")),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        })
    }
}

/// One `[user.NAME]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub role: Role,
    /// The password's argon2 hash, a PHC string.
    pub password: String,
    /// The CompIDs the user may act on; empty, all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<Token>,
}

/// A token the user may authenticate with: kept as its hash, and an id to revoke it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub id: String,
    pub sha256: String,
}

/// `users.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Users {
    #[serde(default)]
    user: BTreeMap<String, User>,
}

impl Users {
    /// Reads and checks the users file at `path`.
    ///
    /// # Errors
    ///
    /// The file can't be read, isn't TOML of the shape above, or has a bad name, password hash or
    /// token hash.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        text.parse().map_err(|e| format!("{}: {e}", path.display()))
    }

    /// As [`load`](Self::load), but no users if the file doesn't exist: the first `user add`
    /// creates it.
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load), for a file that exists.
    pub fn load_or_new(path: &Path) -> Result<Self, String> {
        match path.try_exists() {
            Ok(false) => Ok(Self::default()),
            _ => Self::load(path),
        }
    }

    /// Writes the users to `path` whole, through a temporary file renamed over it, so a reader
    /// never sees half a file. The file keeps its permissions; a new one is readable by its
    /// owner only, on Unix.
    ///
    /// # Errors
    ///
    /// Writing or renaming fails.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = Path::new(&temporary);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(temporary)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temporary, path)
    }

    /// The user called `name`.
    pub fn get(&self, name: &str) -> Option<&User> {
        self.user.get(name)
    }

    /// The users' names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.user.keys().map(String::as_str)
    }

    /// Adds user `name`, hashing `password`.
    ///
    /// # Errors
    ///
    /// The name isn't one a user may have, the user exists, or the password is empty.
    pub fn add(&mut self, name: &str, role: Role, clients: Vec<String>, password: &str) -> Result<(), String> {
        check_name(name)?;
        if self.user.contains_key(name) {
            return Err(format!("user {name:?} exists: remove it first"));
        }
        if password.is_empty() {
            return Err("the password is empty".to_string());
        }
        if clients.iter().any(String::is_empty) {
            return Err("an empty client CompID".to_string());
        }
        self.user
            .insert(name.to_string(), User { role, password: hash_password(password), clients, tokens: Vec::new() });
        Ok(())
    }

    /// Removes user `name`, returning whether there was one.
    pub fn remove(&mut self, name: &str) -> bool {
        self.user.remove(name).is_some()
    }

    /// Gives user `name` a new token, returning it: the only time it's seen, as only its hash is
    /// kept.
    ///
    /// # Errors
    ///
    /// No such user.
    pub fn add_token(&mut self, name: &str) -> Result<String, String> {
        let user = self.user.get_mut(name).ok_or_else(|| format!("no user {name:?}"))?;
        loop {
            let token = format!("{TOKEN_PREFIX}{}", Base64UrlUnpadded::encode_string(&random::<TOKEN_BYTES>()));
            let id = token_id(&token).to_string();
            // Ids are random: two of a user's tokens sharing one is unlikely, but would make
            // revoking one by its id ambiguous.
            if user.tokens.iter().all(|t| t.id != id) {
                user.tokens.push(Token { id, sha256: sha256_hex(&token) });
                return Ok(token);
            }
        }
    }

    /// Revokes user `name`'s token whose id starts with `prefix`: the token whole, its id, or
    /// the id's first characters, with or without `jb_`. Returns the revoked token's id.
    ///
    /// # Errors
    ///
    /// No such user, or no token or more than one matches.
    pub fn revoke_token(&mut self, name: &str, prefix: &str) -> Result<String, String> {
        let user = self.user.get_mut(name).ok_or_else(|| format!("no user {name:?}"))?;
        let prefix = token_id(prefix);
        if prefix.is_empty() {
            return Err("an empty token prefix".to_string());
        }
        let matching: Vec<usize> =
            user.tokens.iter().enumerate().filter(|(_, t)| t.id.starts_with(prefix)).map(|(i, _)| i).collect();
        match matching.as_slice() {
            [index] => Ok(user.tokens.remove(*index).id),
            [] => Err(format!("user {name:?} has no token starting {prefix:?}")),
            _ => {
                let ids: Vec<&str> = matching.iter().map(|&i| user.tokens[i].id.as_str()).collect();
                Err(format!("{prefix:?} matches more than one token of {name:?}: {ids:?}"))
            }
        }
    }

    /// The user whose token `token` is, and their name. Compares the token's hash with every
    /// token's in constant time, so how long it takes says nothing of how close a guess was.
    pub fn by_token(&self, token: &str) -> Option<(&str, &User)> {
        let hash = sha256_hex(token);
        let mut found = None;
        for (name, user) in &self.user {
            for t in &user.tokens {
                if bool::from(t.sha256.as_bytes().ct_eq(hash.as_bytes())) {
                    found = Some((name.as_str(), user));
                }
            }
        }
        found
    }

    fn check(&self) -> Result<(), String> {
        for (name, user) in &self.user {
            check_name(name)?;
            PasswordHash::new(&user.password)
                .map_err(|e| format!("user {name:?}: the password isn't a PHC hash: {e}"))?;
            if user.clients.iter().any(String::is_empty) {
                return Err(format!("user {name:?}: an empty client CompID"));
            }
            for token in &user.tokens {
                let hex = token.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if token.sha256.len() != 64 || !hex {
                    return Err(format!("user {name:?}: token {:?}: sha256 isn't 64 lowercase hex digits", token.id));
                }
                if token.id.is_empty() {
                    return Err(format!("user {name:?}: a token with an empty id"));
                }
            }
        }
        Ok(())
    }
}

impl FromStr for Users {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let users: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        users.check()?;
        Ok(users)
    }
}

/// A user name is 1 to [`MAX_NAME_LEN`] letters, digits and `.`, `_`, `-` or `@`: an email
/// address works, and names print plainly in logs and the audit file.
fn check_name(name: &str) -> Result<(), String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@');
    if name.is_empty() || name.len() > MAX_NAME_LEN || !name.chars().all(allowed) {
        return Err(format!("user name {name:?}: 1 to {MAX_NAME_LEN} letters, digits, '.', '_', '-' or '@'"));
    }
    Ok(())
}

/// The id a token is revoked by: its first characters after the prefix. Also cuts a prefix the
/// operator typed down to what ids hold.
pub fn token_id(token: &str) -> &str {
    let token = token.strip_prefix(TOKEN_PREFIX).unwrap_or(token);
    token.get(..TOKEN_ID_LEN).unwrap_or(token)
}

/// `password`'s argon2id hash, with argon2's default cost, as a PHC string.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::encode_b64(&random::<16>()).expect("16 bytes is a valid salt");
    Argon2::default().hash_password(password.as_bytes(), &salt).expect("the default parameters are valid").to_string()
}

/// Whether `password` hashes to `phc`. Slow on purpose: call it off the runtime's threads.
pub fn verify_password(phc: &str, password: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
}

/// `N` bytes from the operating system's random source.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source");
    bytes
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_ordered_and_parse() {
        assert!(Role::Viewer < Role::Operator);
        assert!(Role::Operator < Role::Admin);
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            assert_eq!(role.to_string().parse(), Ok(role));
        }
        assert!("root".parse::<Role>().is_err());
    }

    #[test]
    fn an_empty_file_has_no_users() {
        let users: Users = "# nobody yet\n".parse().unwrap();
        assert_eq!(users.names().count(), 0);
    }

    /// Unknown keys, bad roles, names and hashes are refused.
    #[test]
    fn bad_files_are_refused() {
        let hash = hash_password("pw");
        let user = |more: &str| format!("[user.a]\nrole = \"viewer\"\npassword = \"{hash}\"\n{more}");
        assert!(user("").parse::<Users>().is_ok());
        for bad in [
            user("colour = \"red\""),
            user("[[user.a.tokens]]\nid = \"x\"\nsha256 = \"abc\""),
            user("[[user.a.tokens]]\nid = \"x\"\nsha256 = \"abc\"\nnote = \"\""),
            "[user.a]\nrole = \"root\"\npassword = \"x\"".to_string(),
            "[user.a]\nrole = \"viewer\"\npassword = \"plain text\"".to_string(),
            format!("[user.\"a b\"]\nrole = \"viewer\"\npassword = \"{hash}\""),
            "[users.a]".to_string(),
        ] {
            assert!(bad.parse::<Users>().is_err(), "{bad}");
        }
    }

    /// A user added, saved and loaded back has their password, and their token authenticates
    /// until revoked by its id's first characters.
    #[test]
    fn add_save_load_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.toml");
        let mut users = Users::load_or_new(&path).unwrap();
        users.add("alice", Role::Operator, vec!["FUNDA".to_string()], "secret").unwrap();
        assert!(users.add("alice", Role::Admin, Vec::new(), "x").is_err());
        assert!(users.add("bob", Role::Admin, Vec::new(), "").is_err());
        let token = users.add_token("alice").unwrap();
        assert!(token.starts_with("jb_"));
        assert_eq!(token.len(), 3 + 43);
        users.save(&path).unwrap();

        let mut users = Users::load(&path).unwrap();
        let alice = users.get("alice").unwrap();
        assert!(verify_password(&alice.password, "secret"));
        assert!(!verify_password(&alice.password, "Secret"));
        assert_eq!(users.by_token(&token).map(|(name, _)| name), Some("alice"));
        assert_eq!(users.by_token("jb_nope"), None);

        assert!(users.revoke_token("alice", "zzzzzzzz").is_err());
        let id = users.revoke_token("alice", &token[..7]).unwrap();
        assert_eq!(id, &token[3..11]);
        assert_eq!(users.by_token(&token), None);
        assert!(users.remove("alice"));
        assert!(!users.remove("alice"));
    }

    /// The whole token, its id, and a prefix of either all revoke it; a prefix two share doesn't.
    #[test]
    fn revoking_by_an_ambiguous_prefix_is_refused() {
        let mut users = Users::default();
        users.add("a", Role::Viewer, Vec::new(), "pw").unwrap();
        let tokens = [
            Token { id: "abcd1111".into(), sha256: String::new() },
            Token { id: "abcd2222".into(), sha256: String::new() },
        ];
        users.user.get_mut("a").unwrap().tokens = tokens.to_vec();
        assert!(users.revoke_token("a", "abcd").unwrap_err().contains("more than one"));
        assert_eq!(users.revoke_token("a", "jb_abcd2222rest-of-token"), Ok("abcd2222".to_string()));
        assert_eq!(users.revoke_token("a", "abc"), Ok("abcd1111".to_string()));
    }

    /// The file's permissions survive a rewrite; a new file is its owner's only.
    #[cfg(unix)]
    #[test]
    fn saving_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.toml");
        Users::default().save(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        Users::default().save(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
    }
}
