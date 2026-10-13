//! The audit log: what the operator API's callers did, and what they were refused, one JSON line
//! each, fsynced before the action runs.
//!
//! ```json
//! {"time":"2026-10-11T09:30:00.123456Z","user":"alice","role":"operator","ip":"192.0.2.1","action":"block","target":"comp:FUNDA","params":null,"reason":"runaway algo","result":"allowed"}
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
//! **The files.** A day's lines go in `audit-YYYY-MM-DD.jsonl` in the log's directory, the day
//! being the entry's `time` in UTC, which is what reads filter on. The directory is created
//! readable by its owner only if missing. A file is opened by path for each line, appending, so a
//! file moved away or deleted is created afresh rather than written to unseen; a new file is
//! created readable by its owner only, and its directory synced. One line is written at a time, on
//! a blocking thread. With a retention, the first line of each day deletes the day files whose
//! whole day ended longer ago than that; nothing else in the directory is touched. At open, a last
//! line cut short (by a crash) in the newest day's file is ended, so the next line starts on its
//! own: a crash tears only the file being written, the newest. A read goes back from the newest
//! day only as far as it needs.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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

/// The audit log in a directory; see the module documentation.
#[derive(Debug)]
pub struct Audit {
    dir: PathBuf,
    /// How long after its day ends a day's file is kept; for ever if `None`.
    retention: Option<TimeDelta>,
    /// Held while a line is written, by the blocking thread writing it: a request given up on
    /// midway can't let another line in before its own is done. It holds the day last written,
    /// whose first line deletes the files past the retention; `None` until a line is written, so
    /// the first after a start deletes them too.
    writer: Arc<tokio::sync::Mutex<Option<NaiveDate>>>,
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
    /// The audit log in `dir`, created if missing, the newest day's torn last line ended, keeping
    /// day files `retention` after their day ends (for ever if `None`). Warns if others may read
    /// or write the directory.
    ///
    /// # Errors
    ///
    /// The directory can't be created or listed, the newest file can't be read or ended, or
    /// `retention` is too long to count with.
    pub fn open(dir: &Path, retention: Option<Duration>) -> io::Result<Self> {
        let retention = retention.map(TimeDelta::from_std).transpose().map_err(io::Error::other)?;
        create_dir(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir)?.permissions().mode();
            if mode & 0o077 != 0 {
                warn!(dir = %dir.display(), "the audit log is open to others (mode {:o}): chmod 700 it", mode & 0o777);
            }
        }
        if let Some((_, newest)) = day_files(dir)?.pop() {
            end_torn_line(&newest)?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            retention,
            writer: Arc::new(tokio::sync::Mutex::new(None)),
            refusals: Mutex::new(Budget { start: Instant::now(), written: 0, skipped: 0 }),
        })
    }

    /// Appends `entry` to its day's file and syncs it to disk. The first line of a day then
    /// deletes the files past the retention: a failure there is a warning, as the line is written.
    ///
    /// # Errors
    ///
    /// It couldn't be written or synced: the action it records mustn't run.
    pub async fn record(&self, entry: &Entry) -> io::Result<()> {
        let line = entry.line()?;
        let (time, day) = (entry.time, entry.time.date_naive());
        let path = self.dir.join(file_name(day));
        let (dir, retention) = (self.dir.clone(), self.retention);
        let mut last = self.writer.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let mut file = open_append(&path)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()?;
            // Only forward: a line stamped just before midnight but written just after is no new day.
            if last.is_none_or(|last| last < day) {
                *last = Some(day);
                if let Some(retention) = retention {
                    delete_old(&dir, time, retention);
                }
            }
            Ok(())
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
    /// over. No directory yet is no entries.
    ///
    /// # Errors
    ///
    /// The directory or a day's file can't be read.
    pub async fn read(&self, since: Option<DateTime<Utc>>, limit: usize) -> io::Result<Vec<Entry>> {
        let dir = self.dir.clone();
        let limit = limit.min(MAX_READ);
        tokio::task::spawn_blocking(move || {
            let mut entries = VecDeque::with_capacity(limit);
            // ponytail: reads only the days it needs, newest first, but each of them whole; read a
            // day from its end if one day's file grows large.
            for (day, path) in day_files(&dir)?.into_iter().rev() {
                if entries.len() == limit {
                    break;
                }
                // Older days ended sooner still.
                if since.is_some_and(|since| day_end(day).is_some_and(|end| end <= since)) {
                    break;
                }
                for entry in read_day(&path, since, limit - entries.len())?.into_iter().rev() {
                    entries.push_front(entry);
                }
            }
            debug_assert!(entries.len() <= limit);
            Ok(entries.into())
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// A day's file's name.
fn file_name(day: NaiveDate) -> String {
    format!("audit-{}.jsonl", day.format("%Y-%m-%d"))
}

/// The day a file's `name` is for, if it's a day's file: exactly as [`file_name`] writes it.
fn day_of(name: &str) -> Option<NaiveDate> {
    let day = NaiveDate::parse_from_str(name.strip_prefix("audit-")?.strip_suffix(".jsonl")?, "%Y-%m-%d").ok()?;
    (file_name(day) == name).then_some(day)
}

/// When `day` ends, UTC: `None` for the last day chrono counts, which never does.
fn day_end(day: NaiveDate) -> Option<DateTime<Utc>> {
    Some(day.succ_opt()?.and_time(NaiveTime::MIN).and_utc())
}

/// The day files in `dir`, oldest first; none if it's missing. Other files are left out.
fn day_files(dir: &Path) -> io::Result<Vec<(NaiveDate, PathBuf)>> {
    let listing = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        listing => listing?,
    };
    let mut days = Vec::new();
    for file in listing {
        let file = file?;
        if let Some(day) = file.file_name().to_str().and_then(day_of) {
            days.push((day, file.path()));
        }
    }
    days.sort_unstable();
    Ok(days)
}

/// The last `need` entries at or after `since` in a day's file at `path`; none if it's gone (moved
/// away, or deleted past the retention, since it was listed).
fn read_day(path: &Path, since: Option<DateTime<Utc>>, need: usize) -> io::Result<VecDeque<Entry>> {
    debug_assert!(need > 0);
    let file = match File::open(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(VecDeque::new()),
        opened => opened?,
    };
    let mut reader = BufReader::new(file);
    let mut entries = VecDeque::with_capacity(need);
    let mut line = Vec::with_capacity(MAX_LINE_LEN);
    let most = u64::try_from(MAX_LINE_LEN).expect("a usize fits a u64") + 1;
    loop {
        line.clear();
        if (&mut reader).take(most).read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.len() > MAX_LINE_LEN {
            // A line of exactly MAX_LINE_LEN + 1 bytes ended in the take: skipping on would
            // swallow the next one.
            if line.last() != Some(&b'\n') {
                reader.skip_until(b'\n')?;
            }
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(&line) else { continue };
        if since.is_some_and(|since| entry.time < since) {
            continue;
        }
        if entries.len() == need {
            entries.pop_front();
        }
        entries.push_back(entry);
    }
    debug_assert!(entries.len() <= need);
    Ok(entries)
}

/// Deletes the day files in `dir` whose whole day ended more than `retention` before `now`. A
/// failure is a warning only: the line that called for it is written.
fn delete_old(dir: &Path, now: DateTime<Utc>, retention: TimeDelta) {
    let Some(cutoff) = now.checked_sub_signed(retention) else { return };
    let days = match day_files(dir) {
        Ok(days) => days,
        Err(e) => {
            warn!(dir = %dir.display(), "cannot list the audit log's old days to delete: {e}");
            return;
        }
    };
    for (day, path) in days {
        // Oldest first: the rest ended later.
        if day_end(day).is_none_or(|end| end >= cutoff) {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => info!(path = %path.display(), "deleted an audit log day past its retention"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => warn!(path = %path.display(), "cannot delete an audit log day past its retention: {e}"),
        }
    }
}

/// `dir` created owner-only if missing, with its parent synced so it's as durable as its files.
fn create_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    sync_parent(dir)
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
                sync_parent(path)?;
                Ok(file)
            }
        },
        opened => opened,
    }
}

/// Syncs the directory holding `path`, so a name just created there survives a crash. Unix only:
/// elsewhere a directory can't be opened to sync.
fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let directory = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        File::open(directory)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Ends the file's last line, if a crash left it without its newline, so the next line written
/// isn't glued to it and lost with it.
fn end_torn_line(path: &Path) -> io::Result<()> {
    let mut reader = File::open(path)?;
    if reader.metadata()?.len() == 0 {
        return Ok(());
    }
    reader.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    reader.read_exact(&mut last)?;
    if last != *b"\n" {
        warn!(path = %path.display(), "the audit log's last line was cut short: ending it");
        let mut file = open_append(path)?;
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

    /// The file of `day`, `YYYY-MM-DD`, in `dir`.
    fn day(dir: &Path, day: &str) -> PathBuf {
        dir.join(format!("audit-{day}.jsonl"))
    }

    /// The text of the day files in `dir`, oldest first; empty if there are none.
    pub(crate) fn text(dir: &Path) -> String {
        let days = day_files(dir).unwrap();
        days.iter().map(|(_, path)| std::fs::read_to_string(path).unwrap()).collect()
    }

    /// Lines are appended, one JSON object each, to a file created owner-only in a directory
    /// created owner-only; they read back oldest first, from `since`, the last `limit` of them, a
    /// torn line passed over.
    #[tokio::test]
    async fn lines_append_and_read_back() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("audit/log");
        let audit = Audit::open(&dir, None).unwrap();
        let times = ["2026-10-11T09:00:00Z", "2026-10-11T10:00:00Z", "2026-10-11T11:00:00Z"];
        for time in times {
            audit.record(&entry(time, "allowed")).await.unwrap();
        }
        let path = day(&dir, "2026-10-11");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
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

    /// Each day's lines land in its own file, by the entry's time in UTC. A read across days
    /// returns the last `limit` in order, and doesn't open a day that ended before `since`.
    #[tokio::test]
    async fn days_land_in_their_own_files_and_read_back_across_them() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(dir.path(), None).unwrap();
        let times = [
            "2026-10-10T09:00:00Z",
            "2026-10-10T23:59:59Z",
            "2026-10-11T00:00:00Z",
            "2026-10-11T10:00:00Z",
            "2026-10-12T02:00:00+05:00",
            "2026-10-12T09:00:00Z",
        ];
        for time in times {
            audit.record(&entry(time, "allowed")).await.unwrap();
        }
        let lines = |date: &str| std::fs::read_to_string(day(dir.path(), date)).unwrap().lines().count();
        assert_eq!([lines("2026-10-10"), lines("2026-10-11"), lines("2026-10-12")], [2, 3, 1]);
        let entries = |times: &[&str]| times.iter().map(|time| entry(time, "allowed")).collect::<Vec<_>>();
        let read = |since: Option<&str>, limit| audit.read(since.map(|s| s.parse().unwrap()), limit);
        assert_eq!(read(None, MAX_READ).await.unwrap(), entries(&times));
        assert_eq!(read(None, 4).await.unwrap(), entries(&times[2..]));
        assert_eq!(read(None, 2).await.unwrap(), entries(&times[4..]));
        assert_eq!(read(Some("2026-10-10T12:00:00Z"), 3).await.unwrap(), entries(&times[3..]));
        assert_eq!(read(Some("2026-10-10T12:00:00Z"), MAX_READ).await.unwrap(), entries(&times[1..]));

        // A line in the 10th's file stamped the 11th would be read, were the file opened.
        let stray = serde_json::to_string(&entry("2026-10-11T12:00:00Z", "stray")).unwrap();
        let tenth = day(dir.path(), "2026-10-10");
        std::fs::write(&tenth, format!("{}{stray}\n", std::fs::read_to_string(&tenth).unwrap())).unwrap();
        assert_eq!(read(Some("2026-10-11T00:00:00Z"), MAX_READ).await.unwrap(), entries(&times[2..]));
        assert_eq!(read(Some("2026-10-10T23:59:59Z"), MAX_READ).await.unwrap().len(), 6);
    }

    /// With a retention, a new day's first line deletes the day files whose whole day ended more
    /// than the retention before it; other lines, younger days and other files are left alone.
    #[tokio::test]
    async fn a_new_day_deletes_the_days_past_the_retention() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}\n", serde_json::to_string(&entry("2026-10-01T09:00:00Z", "allowed")).unwrap());
        let days = ["audit-2026-10-01.jsonl", "audit-2026-10-08.jsonl", "audit-2026-10-09.jsonl"];
        let others = ["audit-2026-1-1.jsonl", "audit-2026-10-01.jsonl.gz", "notes.txt"];
        for name in days.iter().chain(&others) {
            std::fs::write(dir.path().join(name), &line).unwrap();
        }
        let names = || {
            let mut names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|file| file.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let audit = Audit::open(dir.path(), Some(Duration::from_secs(2 * 86_400))).unwrap();
        // The 8th ended at the 9th's midnight, over two days before; the 9th's end isn't.
        audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.unwrap();
        let with = |kept: &[&str]| {
            let mut names: Vec<_> = others.iter().chain(kept).map(ToString::to_string).collect();
            names.sort();
            names
        };
        assert_eq!(names(), with(&["audit-2026-10-09.jsonl", "audit-2026-10-11.jsonl"]));

        std::fs::write(dir.path().join("audit-2026-10-01.jsonl"), &line).unwrap();
        audit.record(&entry("2026-10-11T10:00:00Z", "allowed")).await.unwrap();
        assert!(dir.path().join("audit-2026-10-01.jsonl").exists());
        audit.record(&entry("2026-10-12T09:00:00Z", "allowed")).await.unwrap();
        assert_eq!(names(), with(&["audit-2026-10-11.jsonl", "audit-2026-10-12.jsonl"]));
    }

    /// A last line torn by a crash, in the newest day's file, is ended at open, so the next line
    /// reads back whole; an older day's file isn't touched. A missing file reads as empty.
    #[tokio::test]
    async fn a_torn_last_line_is_ended_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(dir.path(), None).unwrap();
        audit.record(&entry("2026-10-10T09:00:00Z", "allowed")).await.unwrap();
        audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.unwrap();
        let (older, newest) = (day(dir.path(), "2026-10-10"), day(dir.path(), "2026-10-11"));
        for path in [&older, &newest] {
            let text = std::fs::read_to_string(path).unwrap();
            std::fs::write(path, format!("{text}{{\"time\":\"2026-")).unwrap();
        }
        let audit = Audit::open(dir.path(), None).unwrap();
        assert!(!std::fs::read_to_string(&older).unwrap().ends_with('\n'));
        audit.record(&entry("2026-10-11T10:00:00Z", "allowed")).await.unwrap();
        let read = audit.read(Some("2026-10-11T00:00:00Z".parse().unwrap()), MAX_READ).await.unwrap();
        assert_eq!(read, [entry("2026-10-11T09:00:00Z", "allowed"), entry("2026-10-11T10:00:00Z", "allowed")]);
        std::fs::remove_dir_all(dir.path()).unwrap();
        assert!(audit.read(None, MAX_READ).await.unwrap().is_empty());
    }

    /// A line over the limit has its params replaced by their length and its strings cut, at a
    /// character's boundary; reading passes over a longer line written by someone else.
    #[tokio::test]
    async fn lines_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = day(dir.path(), "2026-10-11");
        let audit = Audit::open(dir.path(), None).unwrap();
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

    /// A day's file deleted (or moved away) is created afresh at its next line.
    #[tokio::test]
    async fn a_deleted_file_is_created_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = day(dir.path(), "2026-10-11");
        let audit = Audit::open(dir.path(), None).unwrap();
        audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.unwrap();
        std::fs::rename(&path, dir.path().join("moved.jsonl")).unwrap();
        audit.record(&entry("2026-10-11T10:00:00Z", "allowed")).await.unwrap();
        assert_eq!(audit.read(None, MAX_READ).await.unwrap(), [entry("2026-10-11T10:00:00Z", "allowed")]);
    }

    /// A file that can't be written, or a directory gone, fails the write; a directory that
    /// can't be created fails opening.
    #[tokio::test]
    async fn a_write_that_fails_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(dir.path(), None).unwrap();
        audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.unwrap();
        let unwritable = make_unwritable(dir.path());
        assert!(audit.record(&entry("2026-10-11T10:00:00Z", "allowed")).await.is_err());
        assert!(audit.record(&entry("2026-10-12T10:00:00Z", "allowed")).await.is_err());
        drop(unwritable);
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("file"), "").unwrap();
        assert!(Audit::open(&other.path().join("file/audit"), None).is_err());
        let gone = other.path().join("gone");
        let audit = Audit::open(&gone, None).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        assert!(audit.record(&entry("2026-10-11T09:00:00Z", "allowed")).await.is_err());
    }

    /// Refusals are written up to the budget a minute, then logged only, until the minute passes.
    #[test]
    fn refusals_are_written_within_a_budget() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(dir.path(), None).unwrap();
        let start = Instant::now();
        for _ in 0..REFUSALS_PER_MINUTE {
            assert!(audit.spend_refusal(start));
        }
        assert!(!audit.spend_refusal(start));
        assert!(!audit.spend_refusal(start + Duration::from_secs(59)));
        assert!(audit.spend_refusal(start + Duration::from_secs(60)));
    }

    /// Makes the audit log in `dir` unwritable, its files and the directory (so no day's file can
    /// be created), until the guard returned is dropped, so the directory can be cleaned up.
    #[must_use]
    pub(crate) fn make_unwritable(dir: &Path) -> Unwritable<'_> {
        for file in std::fs::read_dir(dir).unwrap() {
            set_readonly(&file.unwrap().path(), true).unwrap();
        }
        set_readonly(dir, true).unwrap();
        Unwritable(dir)
    }

    pub(crate) struct Unwritable<'a>(&'a Path);

    impl Drop for Unwritable<'_> {
        fn drop(&mut self) {
            // The test may have removed the directory meanwhile.
            if set_readonly(self.0, false).is_ok()
                && let Ok(files) = std::fs::read_dir(self.0)
            {
                for file in files.flatten() {
                    let _ = set_readonly(&file.path(), false);
                }
            }
        }
    }

    #[allow(clippy::permissions_set_readonly_false, reason = "restores what make_unwritable took, in a test")]
    fn set_readonly(path: &Path, readonly: bool) -> io::Result<()> {
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_readonly(readonly);
        std::fs::set_permissions(path, permissions)
    }
}
