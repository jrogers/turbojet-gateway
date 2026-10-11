//! The audit log: what the operator API's callers did, and what they were refused, one JSON line
//! each, fsynced before the action runs.
//!
//! ```json
//! {"time":"2026-10-11T09:30:00.123456Z","user":"alice","role":"operator","ip":"192.0.2.1","action":"block","target":"FUNDA","params":null,"reason":"runaway algo","result":"allowed"}
//! ```
//!
//! `result` is `allowed` or `refused: <why>`. An allowed action that didn't simply get done has a
//! second line saying what happened instead: `failed: <why>` (a reload refused, a cancel of an
//! order already done, an action that panicked), or what the action reports, such as
//! `unchanged: already blocked`. An action that got done has no second line.
//!
//! **What is recorded.** Every mutation, every login and logout, allowed or refused, is written
//! before it runs, and if the line can't be written it doesn't run. Reads aren't recorded. Of the
//! refusals of unauthenticated requests, a wrong password or token, a missing or wrong CSRF token
//! and a login refused by the login limit (429, a name or address that failed too often, or the
//! limit full) are recorded: those are someone trying credentials. A login's name is recorded
//! only if it's a user's, else as `<unknown>` (the tracing log has it): a password typed into the
//! name field mustn't land here. A request with no credentials at all, or with a session that has
//! ended, is logged (tracing) only: a console left open past its session asks every few seconds,
//! and recording that would bury the attacks. An identified caller refused for their role or
//! scope, or a mutation without a reason, is recorded too.
//!
//! **What refusals may cost.** Allowed actions, and the refusals of identified callers (whose user
//! and role are known: they're accountable for them), are always written. Refusals of anyone else
//! (a CSRF failure too, whose request can't be trusted to be the user's), which anyone can send as
//! fast as they like, are written at most [`REFUSALS_PER_MINUTE`] a minute, the rest logged only
//! (with a warning when the budget runs out), so a flood of them can't grow the file faster than
//! that, nor queue the API's mutations behind fsyncs. A line is at most [`MAX_LINE_LEN`]: one
//! longer has its params replaced by their length and its strings cut.
//!
//! **The file.** It's opened by path for each line, appending, so a file moved away or deleted
//! (rotated) is created afresh rather than written to unseen; a new file is created readable by its
//! owner only, and its directory synced. One line is written at a time, on a blocking thread. At
//! open, a last line cut short (by a crash) is ended, so the next line starts on its own.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::Role;

/// Refusals written a minute at most. A few operators mistyping a password need a handful; past
/// this, someone is guessing, and the tracing log keeps the rest. Each costs an fsync, so this
/// also bounds how long a flood can hold up a mutation's line.
pub const REFUSALS_PER_MINUTE: u32 = 120;

/// Entries one read returns at most: a screenful's worth many times over, and a bounded answer.
pub const MAX_READ: usize = 1_000;

/// A line's length at most, in bytes, newline included: a mutation's params and a reason fit many
/// times over. Reads pass over longer lines, so neither side holds more than this of one.
pub const MAX_LINE_LEN: usize = 8 * 1024;

/// What a too-long line's target, reason and result are cut to, in bytes. JSON escapes a byte in
/// six at most, so the three cut, and the rest of a line, stay well within [`MAX_LINE_LEN`].
const CUT_FIELD_LEN: usize = 256;

const _: () = assert!(3 * 6 * CUT_FIELD_LEN + 1024 < MAX_LINE_LEN);

/// One line of the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub time: DateTime<Utc>,
    /// `None` for a refusal before the caller was identified.
    pub user: Option<String>,
    pub role: Option<Role>,
    pub ip: IpAddr,
    /// What was asked: `login`, `logout`, `authenticate` (a refusal before the route ran), or the
    /// mutation's name.
    pub action: String,
    /// What it was asked of: a session, an order, a scope, or for `authenticate` the request.
    pub target: Option<String>,
    /// The request's other arguments, `null` if none. Never secrets: no passwords, tokens or CSRF
    /// tokens.
    pub params: serde_json::Value,
    pub reason: Option<String>,
    pub result: String,
}

impl Entry {
    /// An entry for now, without params, reason or result yet.
    pub fn new(user: Option<String>, role: Option<Role>, ip: IpAddr, action: &str, target: Option<String>) -> Self {
        Self {
            time: Utc::now(),
            user,
            role,
            ip,
            action: action.to_string(),
            target,
            params: serde_json::Value::Null,
            reason: None,
            result: String::new(),
        }
    }

    /// The entry as a line, newline included, at most [`MAX_LINE_LEN`]; see the module
    /// documentation.
    fn line(&self) -> io::Result<String> {
        let mut line = serde_json::to_string(self).map_err(io::Error::other)?;
        if line.len() >= MAX_LINE_LEN {
            let mut cut = self.clone();
            cut.params = serde_json::json!({ "truncated": line.len() });
            for text in [cut.target.as_mut(), cut.reason.as_mut(), Some(&mut cut.result)].into_iter().flatten() {
                cut_at_char(text, CUT_FIELD_LEN);
            }
            line = serde_json::to_string(&cut).map_err(io::Error::other)?;
        }
        line.push('\n');
        assert!(line.len() <= MAX_LINE_LEN, "an audit line of {} bytes", line.len());
        Ok(line)
    }
}

/// Cuts `text` to `max` bytes at most, at a character's boundary.
pub(crate) fn cut_at_char(text: &mut String, max: usize) {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// The audit log at a path; see the module documentation.
#[derive(Debug)]
pub struct Audit {
    path: PathBuf,
    /// Held while a line is written, by the blocking thread writing it: a request given up on
    /// midway can't let another line in before its own is done.
    writer: Arc<tokio::sync::Mutex<()>>,
    refusals: Mutex<Budget>,
}

#[derive(Debug)]
struct Budget {
    start: Instant,
    written: u32,
    /// Refusals logged only, this minute.
    skipped: u32,
}

impl Audit {
    /// The audit log at `path`, created if missing, a torn last line ended. Warns if others may
    /// read or write it.
    ///
    /// # Errors
    ///
    /// It can't be created, opened for appending, or read.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = open_append(path)?;
        end_torn_line(path, &mut file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file.metadata()?.permissions().mode();
            if mode & 0o077 != 0 {
                warn!(path = %path.display(), "the audit log is open to others (mode {:o}): chmod 600 it", mode & 0o777);
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(tokio::sync::Mutex::new(())),
            refusals: Mutex::new(Budget { start: Instant::now(), written: 0, skipped: 0 }),
        })
    }

    /// Appends `entry` and syncs it to disk.
    ///
    /// # Errors
    ///
    /// It couldn't be written or synced: the action it records mustn't run.
    pub async fn record(&self, entry: &Entry) -> io::Result<()> {
        let line = entry.line()?;
        let path = self.path.clone();
        let turn = self.writer.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            let mut file = open_append(&path)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()
        })
        .await
        .map_err(io::Error::other)?
    }

    /// As [`record`](Self::record), for a refusal of someone unidentified: written only within
    /// [`REFUSALS_PER_MINUTE`], else logged only.
    ///
    /// # Errors
    ///
    /// As [`record`](Self::record).
    pub async fn record_refusal(&self, entry: &Entry) -> io::Result<()> {
        if self.spend_refusal(Instant::now()) { self.record(entry).await } else { Ok(()) }
    }

    /// Whether a refusal at `now` may be written. The count of those skipped is reported by the
    /// first refusal after their minute: with none after it, it isn't.
    pub(crate) fn spend_refusal(&self, now: Instant) -> bool {
        let mut budget = self.refusals.lock().unwrap_or_else(PoisonError::into_inner);
        if now.saturating_duration_since(budget.start) >= Duration::from_secs(60) {
            if budget.skipped > 0 {
                warn!(skipped = budget.skipped, "refusals logged only, not in the audit log, in the last minute");
            }
            *budget = Budget { start: now, written: 0, skipped: 0 };
        }
        if budget.written < REFUSALS_PER_MINUTE {
            budget.written += 1;
            return true;
        }
        if budget.skipped == 0 {
            warn!("over {REFUSALS_PER_MINUTE} refusals this minute: the rest are logged only, not in the audit log");
        }
        budget.skipped += 1;
        false
    }

    /// The last `limit` entries (at most [`MAX_READ`]) at or after `since`, in the order written.
    /// Lines that don't parse (one cut short by a crash) or are over [`MAX_LINE_LEN`] are passed
    /// over. No file yet is no entries.
    ///
    /// # Errors
    ///
    /// The file can't be read.
    pub async fn read(&self, since: Option<DateTime<Utc>>, limit: usize) -> io::Result<Vec<Entry>> {
        let path = self.path.clone();
        let limit = limit.min(MAX_READ);
        tokio::task::spawn_blocking(move || {
            let file = match File::open(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                opened => opened?,
            };
            // ponytail: scans the whole file each read; an index, or reading from the end, if it
            // grows large (it isn't rotated yet).
            let mut reader = BufReader::new(file);
            let mut entries = VecDeque::with_capacity(limit);
            let mut line = Vec::with_capacity(MAX_LINE_LEN);
            let most = u64::try_from(MAX_LINE_LEN).expect("a usize fits a u64") + 1;
            loop {
                line.clear();
                if (&mut reader).take(most).read_until(b'\n', &mut line)? == 0 {
                    break;
                }
                if line.len() > MAX_LINE_LEN {
                    // A line of exactly MAX_LINE_LEN + 1 bytes ended in the take: skipping on
                    // would swallow the next one.
                    if line.last() != Some(&b'\n') {
                        reader.skip_until(b'\n')?;
                    }
                    continue;
                }
                let Ok(entry) = serde_json::from_slice::<Entry>(&line) else { continue };
                if since.is_some_and(|since| entry.time < since) {
                    continue;
                }
                if entries.len() == limit {
                    entries.pop_front();
                }
                if limit > 0 {
                    entries.push_back(entry);
                }
            }
            debug_assert!(entries.len() <= limit);
            Ok(entries.into())
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// `path` opened for appending, created owner-only if missing, with its directory synced so the
/// new file's name is as durable as its lines.
fn open_append(path: &Path) -> io::Result<File> {
    let options = |create: bool| {
        let mut options = OpenOptions::new();
        options.append(true).create_new(create);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
    };
    match options(false).open(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => match options(true).open(path) {
            // Created meanwhile, by another writer: it's there to append to.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => options(false).open(path),
            created => {
                let file = created?;
                #[cfg(unix)]
                {
                    let directory = match path.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => parent,
                        _ => Path::new("."),
                    };
                    File::open(directory)?.sync_all()?;
                }
                Ok(file)
            }
        },
        opened => opened,
    }
}

/// Ends the file's last line, if a crash left it without its newline, so the next line written
/// isn't glued to it and lost with it.
fn end_torn_line(path: &Path, file: &mut File) -> io::Result<()> {
    let mut reader = File::open(path)?;
    if reader.metadata()?.len() == 0 {
        return Ok(());
    }
    reader.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    reader.read_exact(&mut last)?;
    if last != *b"\n" {
        warn!(path = %path.display(), "the audit log's last line was cut short: ending it");
        file.write_all(b"\n")?;
        file.sync_data()?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn entry(time: &str, result: &str) -> Entry {
        Entry {
            time: time.parse().unwrap(),
            user: Some("alice".into()),
            role: Some(Role::Operator),
            ip: IpAddr::from([192, 0, 2, 1]),
            action: "block".into(),
            target: Some("FUNDA".into()),
            params: serde_json::json!({}),
            reason: Some("runaway algo".into()),
            result: result.into(),
        }
    }

    /// Lines are appended, one JSON object each, to a file created owner-only; they read back
    /// oldest first, from `since`, the last `limit` of them, a torn line passed over.
    #[tokio::test]
    async fn lines_append_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let times = ["2026-10-11T09:00:00Z", "2026-10-11T10:00:00Z", "2026-10-11T11:00:00Z"];
        for time in times {
            audit.record(&entry(time, "allowed")).await.unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(
            text.starts_with(r#"{"time":"2026-10-11T09:00:00Z","user":"alice","role":"operator","ip":"192.0.2.1","#)
        );
        std::fs::write(&path, format!("{text}{{\"time\":\"2026-10-")).unwrap();

        let read = |since: Option<&str>, limit| audit.read(since.map(|s| s.parse().unwrap()), limit);
        let all = read(None, MAX_READ).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], entry(times[0], "allowed"));
        let since = read(Some("2026-10-11T10:00:00Z"), MAX_READ).await.unwrap();
        assert_eq!(since, [entry(times[1], "allowed"), entry(times[2], "allowed")]);
        assert_eq!(read(None, 1).await.unwrap(), [entry(times[2], "allowed")]);
        assert!(read(None, 0).await.unwrap().is_empty());
    }

    /// A last line torn by a crash is ended at open, so the next line reads back whole; a
    /// missing file reads as empty.
    #[tokio::test]
    async fn a_torn_last_line_is_ended_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).unwrap();
        audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"time\":\"2026-");
        std::fs::write(&path, &text).unwrap();
        let audit = Audit::open(&path).unwrap();
        audit.record(&entry("2026-10-11T10:00:00Z", "allowed")).await.unwrap();
        let read = audit.read(None, MAX_READ).await.unwrap();
        assert_eq!(read, [entry("2026-10-11T09:00:00Z", "allowed"), entry("2026-10-11T10:00:00Z", "allowed")]);
        std::fs::remove_file(&path).unwrap();
        assert!(audit.read(None, MAX_READ).await.unwrap().is_empty());
    }

    /// A line over the limit has its params replaced by their length and its strings cut, at a
    /// character's boundary; reading passes over a longer line written by someone else.
    #[tokio::test]
    async fn lines_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).unwrap();
        let mut big = entry("2026-10-11T09:00:00Z", "allowed");
        big.params = serde_json::json!({ "text": "x".repeat(MAX_LINE_LEN) });
        big.target = Some("é".repeat(MAX_LINE_LEN));
        audit.record(&big).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.len() <= MAX_LINE_LEN);
        let [read] = audit.read(None, MAX_READ).await.unwrap().try_into().unwrap();
        assert!(read.params["truncated"].as_u64().unwrap() > u64::try_from(MAX_LINE_LEN).unwrap());
        assert_eq!(read.target.unwrap(), "é".repeat(CUT_FIELD_LEN / 2));
        assert_eq!(read.result, "allowed");

        let mut long = serde_json::to_string(&big).unwrap();
        long.push('\n');
        std::fs::write(&path, format!("{long}{text}{long}")).unwrap();
        assert_eq!(audit.read(None, MAX_READ).await.unwrap().len(), 1);
        // One byte over, its newline the take's last byte: the next line is still read.
        let just_over = format!("{}\n", "x".repeat(MAX_LINE_LEN));
        std::fs::write(&path, format!("{just_over}{text}")).unwrap();
        assert_eq!(audit.read(None, MAX_READ).await.unwrap().len(), 1);
    }

    /// A file deleted (or rotated away) is created afresh at the next line.
    #[tokio::test]
    async fn a_deleted_file_is_created_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).unwrap();
        audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.unwrap();
        std::fs::rename(&path, dir.path().join("audit.jsonl.1")).unwrap();
        audit.record(&entry("2026-10-11T10:00:00Z", "allowed")).await.unwrap();
        assert_eq!(audit.read(None, MAX_READ).await.unwrap(), [entry("2026-10-11T10:00:00Z", "allowed")]);
    }

    /// A file that can't be written, or a directory gone, fails the write; and opening.
    #[tokio::test]
    async fn a_write_that_fails_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let audit = Audit::open(&path).unwrap();
        make_read_only(&path);
        assert!(audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.is_err());
        assert!(Audit::open(&path).is_err());
        let gone = dir.path().join("gone");
        std::fs::create_dir(&gone).unwrap();
        let audit = Audit::open(&gone.join("audit.jsonl")).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        assert!(audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.is_err());
    }

    /// Refusals are written up to the budget a minute, then logged only, until the minute passes.
    #[test]
    fn refusals_are_written_within_a_budget() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(&dir.path().join("audit.jsonl")).unwrap();
        let start = Instant::now();
        for _ in 0..REFUSALS_PER_MINUTE {
            assert!(audit.spend_refusal(start));
        }
        assert!(!audit.spend_refusal(start));
        assert!(!audit.spend_refusal(start + Duration::from_secs(59)));
        assert!(audit.spend_refusal(start + Duration::from_secs(60)));
    }

    pub(crate) fn make_read_only(path: &Path) {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}
