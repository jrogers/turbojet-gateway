//! The operator API's users: `users.toml`, their roles, passwords and tokens.
//!
//! ```toml
//! [user.alice]
//! role = "operator"                 # viewer, operator or admin; each may do what those before it may
//! password = "$argon2id$v=19$..."   # argon2id, at its default cost or more: `user add` writes it
//! clients = ["FUNDA"]               # optional: acts on these CompIDs only; all, if left out
//!                                   # (an empty list is refused)
//!
//! [[user.alice.tokens]]             # `turbojet-gateway token add` writes these
//! id = "q3Xr9a_T"                   # the token's first characters after `jb_`, to revoke it by
//! sha256 = "5f2c..."                # the token's SHA-256, hex: the token itself isn't kept
//! ```
//!
//! An empty file (or one with only comments) has no users. Unknown keys are errors, and so is a
//! token hash used twice. The binary's
//! `user` and `token` commands edit the file, rewriting it whole (comments aren't kept); the
//! gateway rereads it at start and on reload.

use std::collections::BTreeMap;
use std::fmt;
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

/// A password's length at most, in bytes: well past any passphrase, and short of making argon2
/// hash megabytes for whoever sends them. Longer ones are refused before they're hashed.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// A new password's length at least, in bytes: the operator API can stop a client's trading.
pub const MIN_PASSWORD_LEN: usize = 12;

/// The most memory a password hash may ask argon2 for, in KiB (its `m`): 256 MiB, about thirteen
/// times the default. A hash in the users file that asks more, hand-edited, would make every login
/// for it allocate that much, and the API checks four at once: a gigabyte at this ceiling.
const MAX_HASH_MEMORY_KIB: u32 = 256 * 1024;

/// The most passes a password hash may ask argon2 for (its `t`): ten times the default. With the
/// memory ceiling too, a login takes some seconds of a blocking thread.
const MAX_HASH_PASSES: u32 = 20;

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
    /// The CompIDs the user may act on; `None`, all of them. An empty list is refused: leaving
    /// `clients` out is the only way to say all, so a list emptied by mistake doesn't widen it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<Vec<String>>,
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

    /// Writes the users to `path` whole, as `write_atomically` does: a symlink is followed, and
    /// the warnings are its. Whoever runs this owns the new file, so run it as the gateway's user.
    ///
    /// # Errors
    ///
    /// Resolving the path, writing or renaming fails.
    pub fn save(&self, path: &Path) -> io::Result<Option<String>> {
        write_atomically(path, &toml::to_string(self).map_err(io::Error::other)?)
    }

    /// User `name`'s new password, their role, clients and tokens kept.
    ///
    /// # Errors
    ///
    /// No such user, or the password is too short or too long.
    pub fn set_password(&mut self, name: &str, password: &str) -> Result<(), String> {
        check_new_password(password)?;
        let user = self.user.get_mut(name).ok_or_else(|| format!("no user {name:?}"))?;
        user.password = hash_password(password);
        Ok(())
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
    /// The name isn't one a user may have, the user exists, the password is too short or too
    /// long, or a client is empty. No `clients` means all.
    pub fn add(&mut self, name: &str, role: Role, clients: Vec<String>, password: &str) -> Result<(), String> {
        check_name(name)?;
        if self.user.contains_key(name) {
            return Err(format!("user {name:?} exists: remove it first, or change its password with user passwd"));
        }
        check_new_password(password)?;
        if clients.iter().any(String::is_empty) {
            return Err("an empty client CompID".to_string());
        }
        let mut clients = clients;
        sort_clients(&mut clients);
        let clients = (!clients.is_empty()).then_some(clients);
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
    /// token's, in constant time and without stopping at a match, so how long it takes depends
    /// only on how many tokens there are. (A leak would say little anyway: it would be of how
    /// close a guess's SHA-256 came, not the token.) Loading refuses a hash used twice,
    /// so at most one matches.
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
        let mut hashes = std::collections::HashSet::new();
        for (name, user) in &self.user {
            check_name(name)?;
            check_hash(&user.password).map_err(|e| format!("user {name:?}: password: {e}"))?;
            match &user.clients {
                Some(clients) if clients.is_empty() => {
                    return Err(format!("user {name:?}: clients = []: leave clients out to mean all"));
                }
                Some(clients) if clients.iter().any(String::is_empty) => {
                    return Err(format!("user {name:?}: an empty client CompID"));
                }
                _ => {}
            }
            for token in &user.tokens {
                if !hashes.insert(token.sha256.as_str()) {
                    return Err(format!("user {name:?}: token {:?}: another token has the same hash", token.id));
                }
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
        let mut users: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        users.check()?;
        for clients in users.user.values_mut().filter_map(|user| user.clients.as_mut()) {
            sort_clients(clients);
        }
        Ok(users)
    }
}

/// Sorts and deduplicates a user's clients, once, as they're read or added: the same clients
/// listed in another order are then equal, so a reload that only reorders them doesn't make a
/// live socket send a new snapshot, as a change of scope does.
fn sort_clients(clients: &mut Vec<String>) {
    clients.sort_unstable();
    clients.dedup();
}

/// A user name is 1 to [`MAX_NAME_LEN`] letters, digits and `.`, `_`, `-` or `@`: an email
/// address works, and names print plainly in logs and the audit file.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@');
    if name.is_empty() || name.len() > MAX_NAME_LEN || !name.chars().all(allowed) {
        return Err(format!("user name {name:?}: 1 to {MAX_NAME_LEN} letters, digits, '.', '_', '-' or '@'"));
    }
    Ok(())
}

/// Refuses an empty password, or one over [`MAX_PASSWORD_LEN`]. New ones must also be
/// [`MIN_PASSWORD_LEN`] long: see [`check_new_password`].
fn check_password(password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("the password is empty".to_string());
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(format!("the password is over {MAX_PASSWORD_LEN} bytes"));
    }
    Ok(())
}

/// Refuses a new password shorter than [`MIN_PASSWORD_LEN`] or longer than [`MAX_PASSWORD_LEN`].
///
/// # Errors
///
/// It's too short or too long.
pub fn check_new_password(password: &str) -> Result<(), String> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(format!("the password is shorter than {MIN_PASSWORD_LEN} bytes"));
    }
    check_password(password)
}

/// Refuses a hash other than argon2id's, or one cheaper than argon2's defaults (which
/// [`hash_password`] uses): a weak hash is quick to crack if the file leaks, and quicker to check
/// than an unknown user's, which would tell who exists. Refuses one dearer than
/// [`MAX_HASH_MEMORY_KIB`] or [`MAX_HASH_PASSES`] too: each login would cost that. A hash dearer
/// than the defaults but within those is taken, though checking it takes longer than an unknown
/// user's (checked at the defaults), which tells that user exists.
fn check_hash(phc: &str) -> Result<(), String> {
    let hash = PasswordHash::new(phc).map_err(|e| format!("not a PHC hash: {e}"))?;
    if hash.algorithm != argon2::Algorithm::Argon2id.ident() {
        return Err(format!("{} isn't argon2id", hash.algorithm));
    }
    let params = argon2::Params::try_from(&hash).map_err(|e| format!("bad argon2 parameters: {e}"))?;
    if params.m_cost() < argon2::Params::DEFAULT_M_COST
        || params.t_cost() < argon2::Params::DEFAULT_T_COST
        || params.p_cost() < argon2::Params::DEFAULT_P_COST
    {
        return Err(format!(
            "argon2 parameters m={}, t={}, p={} are below the defaults m={}, t={}, p={}",
            params.m_cost(),
            params.t_cost(),
            params.p_cost(),
            argon2::Params::DEFAULT_M_COST,
            argon2::Params::DEFAULT_T_COST,
            argon2::Params::DEFAULT_P_COST,
        ));
    }
    if params.m_cost() > MAX_HASH_MEMORY_KIB || params.t_cost() > MAX_HASH_PASSES {
        return Err(format!(
            "argon2 parameters m={}, t={} are over the most a login may cost, m={MAX_HASH_MEMORY_KIB}, \
             t={MAX_HASH_PASSES}",
            params.m_cost(),
            params.t_cost(),
        ));
    }
    Ok(())
}

/// Writes `text` to `path` whole, through a temporary file in its directory renamed over it, so a
/// reader never sees half a file and a crash leaves the old one or the new. A symlink at `path` is
/// followed: the file it points to is replaced, and the link stays. The file keeps its
/// permissions; a new one is readable by its owner only. Returns a warning if the file's owner
/// changed (whoever runs this owns the new file), or if the directory couldn't be synced once the
/// file was renamed into place: it's written, but a crash could still bring back the old one.
///
/// # Errors
///
/// Resolving the path, writing or renaming fails: the file is as it was.
pub(crate) fn write_atomically(path: &Path, text: &str) -> io::Result<Option<String>> {
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    let old = std::fs::metadata(&path).ok();
    if let Some(old) = &old {
        file.as_file().set_permissions(old.permissions())?;
    }
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    let mut warnings: Vec<_> = owner_change(old.as_ref(), &file.as_file().metadata()?).into_iter().collect();
    file.persist(&path).map_err(|e| e.error)?;
    // The rename itself is durable once the directory is. Past the rename, a failure is a warning:
    // the file is written.
    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(directory).and_then(|directory| directory.sync_all()) {
        warnings.push(format!("{} is written, but syncing its directory failed: {e}", path.display()));
    }
    Ok((!warnings.is_empty()).then(|| warnings.join("; ")))
}

/// A warning if the file a save replaces, `old`, had another owner than the new one.
#[cfg(unix)]
fn owner_change(old: Option<&std::fs::Metadata>, new: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let old = old?;
    (old.uid() != new.uid()).then(|| {
        format!(
            "the file's owner changed from uid {} to uid {}: the gateway may no longer read it; run this as the \
             gateway's user",
            old.uid(),
            new.uid()
        )
    })
}

#[cfg(not(unix))]
fn owner_change(_: Option<&std::fs::Metadata>, _: &std::fs::Metadata) -> Option<String> {
    None
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
    format!("{:x}", Sha256::digest(text.as_bytes()))
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
        users.add("alice", Role::Operator, vec!["FUNDA".to_string()], "alice secret").unwrap();
        assert!(users.add("alice", Role::Admin, Vec::new(), "x").is_err());
        assert!(users.add("bob", Role::Admin, Vec::new(), "").is_err());
        let token = users.add_token("alice").unwrap();
        assert!(token.starts_with("jb_"));
        assert_eq!(token.len(), 3 + 43);
        users.save(&path).unwrap();

        let mut users = Users::load(&path).unwrap();
        let alice = users.get("alice").unwrap();
        assert!(verify_password(&alice.password, "alice secret"));
        assert!(!verify_password(&alice.password, "Alice secret"));
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
        users.add("a", Role::Viewer, Vec::new(), "a long password").unwrap();
        let tokens = [
            Token { id: "abcd1111".into(), sha256: String::new() },
            Token { id: "abcd2222".into(), sha256: String::new() },
        ];
        users.user.get_mut("a").unwrap().tokens = tokens.to_vec();
        assert!(users.revoke_token("a", "abcd").unwrap_err().contains("more than one"));
        assert_eq!(users.revoke_token("a", "jb_abcd2222rest-of-token"), Ok("abcd2222".to_string()));
        assert_eq!(users.revoke_token("a", "abc"), Ok("abcd1111".to_string()));
    }

    /// A user's clients read or added are sorted, once each: the same list reordered is equal.
    #[test]
    fn clients_are_sorted_and_deduplicated() {
        let hash = hash_password("pw");
        let text = format!("[user.a]\nrole = \"viewer\"\npassword = \"{hash}\"\nclients = [\"B\", \"A\", \"B\"]\n");
        let sorted = Some(vec!["A".to_string(), "B".to_string()]);
        assert_eq!(text.parse::<Users>().unwrap().get("a").unwrap().clients, sorted);
        let mut users = Users::default();
        users.add("a", Role::Viewer, vec!["B".into(), "A".into(), "A".into()], "a long password").unwrap();
        assert_eq!(users.get("a").unwrap().clients, sorted);
    }

    /// `clients = []` is refused: leaving it out is the only way to say all.
    #[test]
    fn an_empty_client_list_is_refused() {
        let hash = hash_password("pw");
        let text = format!("[user.a]\nrole = \"viewer\"\npassword = \"{hash}\"\nclients = []\n");
        assert!(text.parse::<Users>().unwrap_err().contains("leave clients out"));
        let mut users = Users::default();
        users.add("a", Role::Viewer, Vec::new(), "a long password").unwrap();
        assert_eq!(users.get("a").unwrap().clients, None);
        assert!(!toml::to_string(&users).unwrap().contains("clients"));
    }

    /// Only argon2id hashes at argon2's default cost or more, and at most the ceiling, are taken.
    #[test]
    fn weak_hashes_are_refused() {
        let user = |hash: &str| format!("[user.a]\nrole = \"viewer\"\npassword = \"{hash}\"\n");
        let salt = SaltString::encode_b64(&[7; 16]).unwrap();
        let weak = argon2::Params::new(1024, 1, 1, None).unwrap();
        let weak = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, weak);
        let weak = weak.hash_password(b"pw", &salt).unwrap().to_string();
        assert!(user(&weak).parse::<Users>().unwrap_err().contains("below the defaults"));
        let argon2i = Argon2::new(argon2::Algorithm::Argon2i, argon2::Version::V0x13, argon2::Params::default());
        let argon2i = argon2i.hash_password(b"pw", &salt).unwrap().to_string();
        assert!(user(&argon2i).parse::<Users>().unwrap_err().contains("isn't argon2id"));
        // Too dear: edited by hand, as hashing one would take a gigabyte and more.
        let dear = |m: u32, t: u32| hash_password("pw").replacen("m=19456,t=2", &format!("m={m},t={t}"), 1);
        assert!(user(&dear(MAX_HASH_MEMORY_KIB, MAX_HASH_PASSES)).parse::<Users>().is_ok());
        assert!(user(&dear(MAX_HASH_MEMORY_KIB + 1, 2)).parse::<Users>().unwrap_err().contains("over the most"));
        assert!(user(&dear(19_456, MAX_HASH_PASSES + 1)).parse::<Users>().unwrap_err().contains("over the most"));
        assert!(user(&hash_password("pw")).parse::<Users>().is_ok());
    }

    /// Two tokens with one hash, under one user or two, are refused.
    #[test]
    fn a_token_hash_used_twice_is_refused() {
        let mut users = Users::default();
        users.add("a", Role::Viewer, Vec::new(), "a long password").unwrap();
        users.add("b", Role::Viewer, Vec::new(), "a long password").unwrap();
        users.add_token("a").unwrap();
        let copy = users.get("a").unwrap().tokens.clone();
        users.user.get_mut("b").unwrap().tokens = copy;
        let text = toml::to_string(&users).unwrap();
        assert!(text.parse::<Users>().unwrap_err().contains("same hash"), "{text}");
    }

    /// A new password keeps the role, clients and tokens; passwords are bounded.
    #[test]
    fn a_new_password_keeps_the_rest() {
        let mut users = Users::default();
        users.add("a", Role::Operator, vec!["FUNDA".into()], "old password").unwrap();
        let token = users.add_token("a").unwrap();
        users.set_password("a", "new password").unwrap();
        let a = users.get("a").unwrap();
        assert!(verify_password(&a.password, "new password"));
        assert_eq!((a.role, a.clients.clone()), (Role::Operator, Some(vec!["FUNDA".to_string()])));
        assert!(users.by_token(&token).is_some());
        assert!(users.set_password("b", "new password").is_err());
        assert!(users.set_password("a", &"x".repeat(MAX_PASSWORD_LEN + 1)).is_err());
        assert!(users.set_password("a", "short").is_err());
        assert!(users.add("b", Role::Viewer, Vec::new(), "short").is_err());
        assert!(check_new_password("short").is_err());
        assert!(check_new_password(&"x".repeat(MIN_PASSWORD_LEN)).is_ok());
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

    /// A save through a symlink writes the file it points to, and the link stays.
    #[cfg(unix)]
    #[test]
    fn saving_follows_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("users.toml");
        std::os::unix::fs::symlink("real.toml", &link).unwrap();
        // Dangling: the file it names doesn't exist yet, so the link itself is written over.
        Users::default().save(&link).unwrap();
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        std::fs::remove_file(&link).unwrap();
        std::fs::write(dir.path().join("real.toml"), "").unwrap();
        std::os::unix::fs::symlink("real.toml", &link).unwrap();
        let mut users = Users::default();
        users.add("ada", Role::Admin, Vec::new(), "a long password").unwrap();
        users.save(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(std::fs::read_to_string(dir.path().join("real.toml")).unwrap().contains("ada"));
    }
}
