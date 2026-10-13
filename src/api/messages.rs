//! Searching the message log [viewer]: `GET /api/messages` by conditions of the caller's, and
//! `GET /api/orders/{order_id}/messages` by an order's IDs. Reads aren't audited.
//!
//! The log has no index. A search reads the files of the days its range touches, oldest first,
//! and answers once it has a page of hits or has read [`SEARCH_BYTES_MAX`], with a `next` cursor
//! to carry on from. The cursor names a file by its day and number, and a record by where it
//! starts in the file, counted before compression: the next page seeks straight there in a `.log`
//! file, and decompresses up to it, without parsing, in a `.log.gz` one.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path as UrlPath, Query};
use axum::http::StatusCode;
use base64ct::{Base64UrlUnpadded, Encoding};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};
use turbojet::codec::{Decoded, decode};
use turbojet::{Direction, FileMessageLog, LogFile, LogFilter, LogRecord, LogRecords, Message};

use super::{ApiError, State};
use crate::book::OrderDetail;
use crate::service::Service;

/// The longest time range one search covers. With the day after's file (see
/// [`LogFilter::covers_day`]) that's three days' files at most, so a cursor's file is one of a
/// handful.
const MAX_RANGE: TimeDelta = TimeDelta::days(1);

/// Hits a page holds without a `limit`: a screenful.
const PAGE_DEFAULT: usize = 100;

/// Hits a page holds at most: each is a message twice over (its fields and its raw form), a few
/// KiB at most, so a page is a few MiB.
const MAX_PAGE: usize = 1_000;

/// Bytes a search reads before answering what it has found: a second or so of reading and
/// decoding, so a search for something rare on a busy day pages rather than holds a thread.
const SEARCH_BYTES_MAX: u64 = 256 * 1024 * 1024;

/// Searches under way at once, each on a blocking thread: a third is answered 503, so searches
/// can't take the threads the gateway's own blocking work (a reload, the users file) needs.
pub(super) const MAX_SEARCHES: usize = 2;

/// Query parameters a search takes at most: each condition is checked against every record read.
const MAX_PARAMS: usize = 100;

/// How long before its first request was sent an order's search starts: the client's order was
/// logged as it arrived, a moment before.
const ORDER_MARGIN: TimeDelta = TimeDelta::minutes(1);

const _: () = assert!(PAGE_DEFAULT <= MAX_PAGE);
// A page reads a record at least, so paging always moves on.
const _: () = assert!(SEARCH_BYTES_MAX > 0);

type AxumState = axum::extract::State<State>;

/// A page of hits, oldest first.
#[derive(Debug, Serialize)]
pub(super) struct Page {
    messages: Vec<Hit>,
    /// The `after` for the next page; `None` once the range is read to its end. A page cut short
    /// by the byte budget may hold no hits and still have a `next`.
    next: Option<String>,
}

/// A message found.
#[derive(Debug, Serialize)]
pub(super) struct Hit {
    time: DateTime<Utc>,
    /// `in` (received) or `out` (sent).
    direction: &'static str,
    /// As logged; `None` before a client's Logon names its session.
    session: Option<String>,
    /// `None`, with no fields, for a frame that doesn't decode.
    msg_type: Option<String>,
    /// `[tag, value]` pairs, as on the wire.
    fields: Vec<(u32, String)>,
    /// As on the wire, `|` for SOH.
    raw: String,
}

/// `GET /api/messages?session=&direction=&msg_type=&tag=&since=&until=&limit=&after=`: the logged
/// messages matching every condition given, oldest first, `limit` at a time (by default
/// [`PAGE_DEFAULT`], at most [`MAX_PAGE`]). `session` (as logged) and `tag` (`11=ABC`) repeat;
/// `msg_type` repeats or takes a list (`D,F`); `direction` is `in` or `out`. `since` (RFC 3339)
/// is required, `until` is `since` and [`MAX_RANGE`] by default, and the range is at most that.
pub(super) async fn messages(
    axum::extract::State(state): AxumState,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Page>, ApiError> {
    // Pairs, not a struct, as `session`, `msg_type` and `tag` repeat.
    let Query(pairs) = query.map_err(|e| bad(e.body_text()))?;
    let dir = log_dir(&*state.service()?)?;
    let search = parse(&pairs)?;
    run(&state, dir, search).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OrderMessagesQuery {
    limit: Option<usize>,
    /// The previous page's `next`.
    after: Option<String>,
}

/// `GET /api/orders/{order_id}/messages?limit=&after=`: the logged messages of the order whose
/// OrderID(37) is `order_id`, both sides', paged as `GET /api/messages`. The client's are those of
/// its session with any of the order's ClOrdIDs in ClOrdID(11) or OrigClOrdID(41); the venue's
/// those of its session with any of the order's downstream ClOrdIDs in either, or the venue's
/// OrderID in OrderID(37). The range is [`MAX_RANGE`] from just before its first request: an
/// order working longer has its later messages found by `GET /api/messages`. A venue whose
/// initiator a reload has stopped has no session to search, so only the client's side is found.
pub(super) async fn order_messages(
    axum::extract::State(state): AxumState,
    order_id: Result<UrlPath<String>, PathRejection>,
    query: Result<Query<OrderMessagesQuery>, QueryRejection>,
) -> Result<Json<Page>, ApiError> {
    let UrlPath(order_id) = order_id.map_err(|e| bad(e.body_text()))?;
    let Query(query) = query.map_err(|e| bad(e.body_text()))?;
    let service = state.service()?;
    let dir = log_dir(&service)?;
    let gateway = service.gateway();
    let detail = gateway
        .order_id(&order_id)
        .and_then(|id| gateway.order(id))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no order {order_id}")))?;
    let venue = service.venue_session(&detail.order.venue).map(|venue| venue.id().to_string());
    drop(service);
    let after = query.after.as_deref().map(Cursor::decode).transpose()?;
    let created = detail.order.created;
    let since = created.checked_sub_signed(ORDER_MARGIN).unwrap_or(created);
    let search = Search::new(order_filters(&detail, venue), since, None, query.limit, after)?;
    run(&state, dir, search).await
}

/// What an order's messages match: see [`order_messages`]. A filter's tag conditions must all
/// hold, so each "or" is a filter of its own.
fn order_filters(detail: &OrderDetail, venue_session: Option<String>) -> Vec<LogFilter> {
    let mut cl_ord_ids: Vec<String> =
        detail.requests.iter().filter_map(|request| request.cl_ord_id.as_deref()).map(str::to_owned).collect();
    cl_ord_ids.push(detail.order.cl_ord_id.to_string());
    cl_ord_ids.sort_unstable();
    cl_ord_ids.dedup();
    let downstreams: Vec<String> = detail.requests.iter().map(|request| request.downstream.to_string()).collect();
    let filter = |session: &str, tag: u32, values: Vec<String>| {
        let mut filter = LogFilter::default();
        filter.sessions = vec![session.to_owned()];
        filter.tags = vec![(tag, values)];
        filter
    };
    let client = detail.order.client.to_string();
    let mut filters = vec![filter(&client, 11, cl_ord_ids.clone()), filter(&client, 41, cl_ord_ids)];
    if let Some(venue) = venue_session {
        filters.push(filter(&venue, 11, downstreams.clone()));
        filters.push(filter(&venue, 41, downstreams));
        if let Some(venue_order_id) = &detail.venue_order_id {
            filters.push(filter(&venue, 37, vec![venue_order_id.to_string()]));
        }
    }
    filters
}

/// The log's directory, or 404 without one.
fn log_dir(service: &Service) -> Result<PathBuf, ApiError> {
    service
        .message_log_dir()
        .map(Path::to_path_buf)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no message log"))
}

/// Runs `search` on a blocking thread, or answers 503 if [`MAX_SEARCHES`] are under way. The
/// thread holds its place until it's done, even if the caller gives up.
async fn run(state: &State, dir: PathBuf, search: Search) -> Result<Json<Page>, ApiError> {
    let permit = state.searches.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{MAX_SEARCHES} searches are under way: try again shortly"),
        )
    })?;
    let scanned = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        scan(&dir, &search, SEARCH_BYTES_MAX)
    })
    .await;
    scanned
        .map_err(|e| {
            error!("a message log search failed: {e}");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "the search failed")
        })?
        .map(Json)
}

/// What a search looks for: records in `range` matching any of `filters`.
#[derive(Debug)]
struct Search {
    /// Without times: `range` has them.
    filters: Vec<LogFilter>,
    /// `filters` without their MsgType and tag conditions: a record none of these passes isn't
    /// decoded.
    headers: Vec<LogFilter>,
    /// Only the times.
    range: LogFilter,
    limit: usize,
    after: Option<Cursor>,
}

impl Search {
    /// A search of `filters` from `since` to `until` (by default `since` and [`MAX_RANGE`]), or
    /// 400 for a range or `limit` out of bounds.
    fn new(
        filters: Vec<LogFilter>,
        since: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
        limit: Option<usize>,
        after: Option<Cursor>,
    ) -> Result<Self, ApiError> {
        let until = match until {
            Some(until) => until,
            None => since.checked_add_signed(MAX_RANGE).ok_or_else(|| bad("since is too late"))?,
        };
        if until <= since {
            return Err(bad("until must be after since"));
        }
        if until - since > MAX_RANGE {
            return Err(bad(format!("a search covers {} hours at most", MAX_RANGE.num_hours())));
        }
        let limit = limit.unwrap_or(PAGE_DEFAULT);
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(bad(format!("limit must be 1 to {MAX_PAGE}")));
        }
        let mut range = LogFilter::default();
        range.since = Some(since);
        range.until = Some(until);
        let headers = filters
            .iter()
            .map(|filter| {
                let mut header = filter.clone();
                header.msg_types.clear();
                header.tags.clear();
                header
            })
            .collect();
        Ok(Self { filters, headers, range, limit, after })
    }
}

/// `GET /api/messages`'s query as a search, or 400: an unknown key, one given twice that can't
/// be, or a value that doesn't parse.
fn parse(pairs: &[(String, String)]) -> Result<Search, ApiError> {
    if pairs.len() > MAX_PARAMS {
        return Err(bad(format!("a search takes {MAX_PARAMS} parameters at most")));
    }
    let mut filter = LogFilter::default();
    let (mut since, mut until, mut limit, mut after) = (None, None, None, None);
    for (key, value) in pairs {
        match key.as_str() {
            "session" => filter.sessions.push(value.clone()),
            "msg_type" => filter.msg_types.extend(value.split(',').map(str::to_owned)),
            "tag" => {
                let (tag, wanted) =
                    value.split_once('=').ok_or_else(|| bad(format!("tag {value:?} isn't TAG=VALUE")))?;
                let tag = tag.parse().map_err(|_| bad(format!("tag {value:?}: {tag:?} isn't a tag number")))?;
                filter.tags.push((tag, vec![wanted.to_owned()]));
            }
            "direction" => {
                let direction = match value.as_str() {
                    "in" => Direction::Inbound,
                    "out" => Direction::Outbound,
                    _ => return Err(bad("direction must be in or out")),
                };
                once(&mut filter.direction, key, direction)?;
            }
            "since" => once(&mut since, key, time(key, value)?)?,
            "until" => once(&mut until, key, time(key, value)?)?,
            "limit" => once(&mut limit, key, value.parse().map_err(|_| bad("limit must be a number"))?)?,
            "after" => once(&mut after, key, Cursor::decode(value)?)?,
            _ => return Err(bad(format!("unknown parameter {key}"))),
        }
    }
    let since = since.ok_or_else(|| bad("since is required"))?;
    Search::new(vec![filter], since, until, limit, after)
}

/// Sets `slot` to `value`, or 400 if `key` was given already.
fn once<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<(), ApiError> {
    if slot.replace(value).is_some() {
        return Err(bad(format!("{key} is given twice")));
    }
    Ok(())
}

fn time(key: &str, value: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.to_utc())
        .map_err(|e| bad(format!("{key} {value:?} isn't an RFC 3339 time: {e}")))
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

/// Where a search carries on from: the record starting at `position` of file `number` of `day`,
/// a [`LogRecords::position`](turbojet::LogRecords::position). The file's name is never taken
/// from the caller: it's found in the directory's listing by day and number, so a cursor can't
/// name a file outside the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    day: NaiveDate,
    number: u32,
    position: u64,
}

impl Cursor {
    /// `DAY-NUMBER-POSITION` in base64url: opaque, so callers pass it back rather than build one.
    fn encode(self) -> String {
        let text = format!("{}-{}-{}", self.day.format("%Y%m%d"), self.number, self.position);
        Base64UrlUnpadded::encode_string(text.as_bytes())
    }

    fn decode(text: &str) -> Result<Self, ApiError> {
        let forged = || bad("after isn't a cursor a search gave");
        let bytes = Base64UrlUnpadded::decode_vec(text).map_err(|_| forged())?;
        let text = String::from_utf8(bytes).map_err(|_| forged())?;
        let mut parts = text.split('-');
        let (Some(day), Some(number), Some(position), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(forged());
        };
        Ok(Self {
            day: NaiveDate::parse_from_str(day, "%Y%m%d").map_err(|_| forged())?,
            number: number.parse().map_err(|_| forged())?,
            position: position.parse().map_err(|_| forged())?,
        })
    }
}

/// Reads the files of `dir` that `search`'s range may be in, from its cursor, until it has a page
/// of hits or has read `bytes_max`: see the module documentation. A cursor that isn't at a record
/// of one of those files is refused (400).
fn scan(dir: &Path, search: &Search, bytes_max: u64) -> Result<Page, ApiError> {
    let files: Vec<LogFile> = list(dir)?.into_iter().filter(|file| search.range.covers_day(file.day)).collect();
    let (first, start) = match search.after {
        None => (0, 0),
        Some(cursor) => {
            let index = files.iter().position(|file| (file.day, file.number) == (cursor.day, cursor.number));
            (index.ok_or_else(gone)?, cursor.position)
        }
    };
    let mut reading = Reading { dir, search, bytes_max, bytes: 0, messages: Vec::new() };
    for (index, file) in files.iter().enumerate().skip(first) {
        let from_cursor = index == first && search.after.is_some();
        let start = if index == first { start } else { 0 };
        if let Some(stopped) = reading.file(file, start, from_cursor)? {
            return Ok(Page { messages: reading.messages, next: Some(stopped.encode()) });
        }
    }
    Ok(Page { messages: reading.messages, next: None })
}

/// The log's files, oldest first. While a file is being compressed, both its `.log` and its
/// `.log.gz` are there: it's kept once, as the `.log.gz` (whole once it's there: it's renamed into
/// place), or as the `.log` without the gzip feature, which can't read the other.
fn list(dir: &Path) -> Result<Vec<LogFile>, ApiError> {
    let mut files = FileMessageLog::files(dir).map_err(|e| {
        error!("cannot list the message log {}: {e}", dir.display());
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cannot list the message log")
    })?;
    files.sort_by_key(|file| (file.day, file.number, file.compressed != cfg!(feature = "gzip")));
    files.dedup_by_key(|file| (file.day, file.number));
    Ok(files)
}

/// A cursor whose file isn't among the range's, or has gone (deleted for its age) since.
fn gone() -> ApiError {
    bad("after names no file of this search's range: start again")
}

/// A scan under way.
struct Reading<'a> {
    dir: &'a Path,
    search: &'a Search,
    bytes_max: u64,
    /// Read so far, before compression: what the records read took up in their files.
    bytes: u64,
    messages: Vec<Hit>,
}

impl Reading<'_> {
    /// Reads `file` from `start`, and where to carry on from if the page ends in it. `from_cursor`:
    /// `start` is the caller's, so a file gone or a bad first record is the cursor's fault.
    fn file(&mut self, file: &LogFile, start: u64, from_cursor: bool) -> Result<Option<Cursor>, ApiError> {
        let Some(mut records) = open(self.dir, file, start, from_cursor)? else { return Ok(None) };
        let mut first = true;
        loop {
            let position = records.position();
            // Before reading, so a page reads a record at least: `bytes` is under the budget at
            // its start.
            if self.bytes.saturating_add(position - start) >= self.bytes_max {
                return Ok(Some(Cursor { day: file.day, number: file.number, position }));
            }
            let record = match records.next() {
                None => break,
                Some(Ok(record)) => record,
                // Not at a record: a position the caller made up.
                Some(Err(_)) if first && start > 0 => return Err(bad("after isn't at a record of its file")),
                // The reader stops at a bad record: the rest of the file can't be found.
                Some(Err(e)) => {
                    warn!("a message log search stops reading {} at a bad record: {e}", file.path.display());
                    break;
                }
            };
            first = false;
            if let Some(hit) = hit(record, self.search) {
                self.messages.push(hit);
                if self.messages.len() >= self.search.limit {
                    let position = records.position();
                    return Ok(Some(Cursor { day: file.day, number: file.number, position }));
                }
            }
        }
        self.bytes = self.bytes.saturating_add(records.position() - start);
        Ok(None)
    }
}

/// `file` of `dir`'s records from `start`, or its `.log.gz`'s if it's been compressed since it was
/// listed (positions hold across compression); `None` for a file deleted since, which only a
/// cursor's file can't be (400), as can't a `start` past its end.
fn open(dir: &Path, file: &LogFile, start: u64, from_cursor: bool) -> Result<Option<LogRecords>, ApiError> {
    let mut opened = FileMessageLog::read_from(&file.path, start);
    if !file.compressed && opened.as_ref().is_err_and(|e| e.kind() == ErrorKind::NotFound) {
        let again = list(dir)?.into_iter().find(|again| (again.day, again.number) == (file.day, file.number));
        if let Some(again) = again {
            opened = FileMessageLog::read_from(&again.path, start);
        }
    }
    match opened {
        Ok(records) => Ok(Some(records)),
        Err(e) if e.kind() == ErrorKind::NotFound && from_cursor => Err(gone()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Err(bad("after is past the end of its file")),
        Err(e) => {
            error!("cannot read the message log's {}: {e}", file.path.display());
            Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cannot read the message log"))
        }
    }
}

/// `record` as a hit, if it's a FIX message in the range matching one of the search's filters.
fn hit(record: LogRecord, search: &Search) -> Option<Hit> {
    let LogRecord::Message { time, direction, session, frame } = record else { return None };
    // FIXP frames: Jetbridge's sessions are all FIX.
    if !search.range.in_range(time) || !frame.starts_with(b"8=") {
        return None;
    }
    if !search.headers.iter().any(|header| header.matches_message(direction, session.as_deref(), None)) {
        return None;
    }
    let msg = match decode(&frame) {
        Decoded::Message(msg, _) => Some(msg),
        _ => None,
    };
    if !search.filters.iter().any(|filter| filter.matches_message(direction, session.as_deref(), msg.as_ref())) {
        return None;
    }
    Some(Hit {
        time,
        direction: match direction {
            Direction::Inbound => "in",
            Direction::Outbound => "out",
        },
        session,
        msg_type: msg.as_ref().and_then(|msg| msg.get(35)).map(str::to_owned),
        fields: msg.as_ref().map_or_else(Vec::new, fields),
        raw: String::from_utf8_lossy(&frame).replace('\x01', "|"),
    })
}

fn fields(msg: &Message) -> Vec<(u32, String)> {
    msg.fields().map(|(tag, value)| (tag, value.to_owned())).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone;
    use turbojet::{Clock, FileLogOptions, MessageLog, SessionId};

    use super::super::read::tests::{Fixture, fixture_with};
    use super::*;

    /// A record to log: when, which way, the session's CompID (`None` for none yet) and the frame.
    type Logged<'a> = (DateTime<Utc>, Direction, Option<&'a str>, Vec<u8>);

    /// Logs `records` to `dir` as one file, of their day: they must share one.
    fn log(dir: &Path, records: &[Logged<'_>]) {
        log_to(dir, false, records);
    }

    /// [`log`], the file compressed if `compress`.
    fn log_to(dir: &Path, compress: bool, records: &[Logged<'_>]) {
        let now = Arc::new(Mutex::new(records[0].0));
        let clock = Clock::from_fn({
            let now = now.clone();
            move || *now.lock().unwrap()
        });
        let log = FileMessageLog::open(dir, FileLogOptions { clock, compress, ..FileLogOptions::default() }).unwrap();
        for (time, direction, comp_id, frame) in records {
            *now.lock().unwrap() = *time;
            let session = comp_id.map(|comp_id| SessionId::new("FIX.4.4", "JETBRIDGE", comp_id));
            match direction {
                Direction::Inbound => log.inbound(session.as_ref(), frame),
                Direction::Outbound => log.outbound(session.as_ref(), frame),
            }
        }
        // Dropping it writes what it holds.
    }

    fn fix(fields: &[(u32, &str)]) -> Vec<u8> {
        let mut all = vec![(8, "FIX.4.4")];
        all.extend_from_slice(fields);
        turbojet::codec::encode(&Message::from_fields(all)).unwrap()
    }

    /// 8 October 2026 at `hour`:`minute` UTC, or a day either side.
    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, minute, 0).unwrap()
    }

    /// A fixture with a message log, and its directory.
    async fn logged() -> (Fixture, PathBuf) {
        let fixture = fixture_with("[message_log]\ndir = \"log\"\n").await;
        let dir = fixture.dirs.0.path().join("log");
        (fixture, dir)
    }

    /// The hits' ClOrdIDs(11) of `GET path`, and its `next`.
    async fn search(fixture: &Fixture, path: &str) -> (Vec<String>, Option<String>) {
        let page = fixture.ok(path).await;
        let ids = page["messages"].as_array().unwrap().iter().map(|hit| cl_ord_id(hit).to_string()).collect();
        (ids, page["next"].as_str().map(str::to_owned))
    }

    fn cl_ord_id(hit: &serde_json::Value) -> &str {
        let fields = hit["fields"].as_array().unwrap();
        fields.iter().find(|field| field[0] == 11).map_or("", |field| field[1].as_str().unwrap())
    }

    const DAY: &str = "since=2026-10-08T00:00:00Z";

    /// Each condition narrows the search: session, direction, MsgType (repeated or listed), tags
    /// (all must hold) and time. FIXP frames and gaps aren't hits.
    #[tokio::test]
    async fn each_condition_narrows_the_search() {
        let (fixture, dir) = logged().await;
        log(
            &dir,
            &[
                (at(8, 9, 0), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, "A"), (55, "AAPL")])),
                (at(8, 9, 1), Direction::Outbound, Some("FUNDA"), fix(&[(35, "8"), (11, "A"), (55, "AAPL")])),
                (at(8, 9, 2), Direction::Outbound, Some("NASDAQ"), fix(&[(35, "D"), (11, "B"), (55, "MSFT")])),
                (at(8, 9, 3), Direction::Inbound, Some("NASDAQ"), b"\x00\x01fixp".to_vec()),
                (at(8, 10, 0), Direction::Inbound, None, fix(&[(35, "F"), (11, "C"), (41, "A")])),
            ],
        );
        let ids = async |query: &str| search(&fixture, &format!("/api/messages?{DAY}{query}")).await.0;
        assert_eq!(ids("").await, ["A", "A", "B", "C"]);
        assert_eq!(ids("&session=FIX.4.4:JETBRIDGE-%3EFUNDA").await, ["A", "A"]);
        assert_eq!(
            ids("&session=FIX.4.4:JETBRIDGE-%3EFUNDA&session=FIX.4.4:JETBRIDGE-%3ENASDAQ").await,
            ["A", "A", "B"]
        );
        assert_eq!(ids("&direction=out").await, ["A", "B"]);
        assert_eq!(ids("&direction=in").await, ["A", "C"]);
        assert_eq!(ids("&msg_type=8").await, ["A"]);
        assert_eq!(ids("&msg_type=8,F").await, ["A", "C"]);
        assert_eq!(ids("&msg_type=8&msg_type=F").await, ["A", "C"]);
        assert_eq!(ids("&tag=55=AAPL").await, ["A", "A"]);
        assert_eq!(ids("&tag=55=AAPL&tag=35=D").await, ["A"]);
        assert_eq!(ids("&tag=41=A").await, ["C"]);
        assert_eq!(ids("&tag=55=IBM").await, Vec::<String>::new());
        let ids = async |query: &str| search(&fixture, &format!("/api/messages?{query}")).await.0;
        assert_eq!(ids("since=2026-10-08T09:01:00Z&until=2026-10-08T09:03:00Z").await, ["A", "B"]);
        assert_eq!(ids("since=2026-10-08T10:00:00%2B01:00").await, ["A", "A", "B", "C"]);

        let page = fixture.ok(&format!("/api/messages?{DAY}&direction=in&session=FIX.4.4:JETBRIDGE-%3EFUNDA")).await;
        let hit = &page["messages"][0];
        assert_eq!(hit["time"], "2026-10-08T09:00:00Z");
        assert_eq!(hit["direction"], "in");
        assert_eq!(hit["session"], "FIX.4.4:JETBRIDGE->FUNDA");
        assert_eq!(hit["msg_type"], "D");
        assert_eq!(hit["fields"][2], serde_json::json!([35, "D"]));
        let raw = hit["raw"].as_str().unwrap();
        assert!(raw.starts_with("8=FIX.4.4|9=") && raw.contains("|35=D|11=A|55=AAPL|10="), "{raw}");
        assert_eq!(page["next"], serde_json::Value::Null);
    }

    /// Bad queries are 400; without a log, both searches are 404.
    #[tokio::test]
    async fn bad_queries_and_no_log() {
        let (fixture, _dir) = logged().await;
        let over = MAX_PAGE + 1;
        for bad in [
            "",
            "since=yesterday",
            "since=2026-10-08T00:00:00Z&until=2026-10-08T00:00:00Z",
            "since=2026-10-08T00:00:00Z&until=2026-10-09T00:00:01Z",
            &format!("{DAY}&limit=0"),
            &format!("{DAY}&limit={over}"),
            &format!("{DAY}&direction=up"),
            &format!("{DAY}&tag=11"),
            &format!("{DAY}&tag=x=1"),
            &format!("{DAY}&{DAY}"),
            &format!("{DAY}&colour=red"),
            &format!("{DAY}{}", "&session=x".repeat(MAX_PARAMS)),
        ] {
            assert_eq!(fixture.get(&format!("/api/messages?{bad}")).await.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let name = fixture.ok("/api/orders").await["orders"][0]["order_id"].as_str().unwrap().to_string();
        let path = format!("/api/orders/{name}/messages?colour=red");
        assert_eq!(fixture.get(&path).await.status, StatusCode::BAD_REQUEST);
        assert_eq!(fixture.get("/api/orders/NOPE1/messages").await.status, StatusCode::NOT_FOUND);

        let unlogged = super::super::read::tests::fixture().await;
        for path in [format!("/api/messages?{DAY}"), format!("/api/orders/{name}/messages")] {
            let answer = unlogged.get(&path).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(answer.body["error"], "no message log");
        }
    }

    /// Only the files of the days the range touches, and the day after each, are read: another
    /// day's file, unreadable, doesn't fail the search, and its messages aren't found.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_ranges_days_are_read() {
        use std::os::unix::fs::PermissionsExt;
        let (fixture, dir) = logged().await;
        for (day, id) in [(6, "A"), (7, "B"), (8, "C"), (9, "D")] {
            log(&dir, &[(at(day, 12, 0), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, id)]))]);
        }
        let ids = async |query: &str| search(&fixture, &format!("/api/messages?{query}")).await.0;
        assert_eq!(ids("since=2026-10-07T00:00:00Z").await, ["B"]);
        let files = FileMessageLog::files(&dir).unwrap();
        let sixth = files.iter().find(|file| file.day == at(6, 0, 0).date_naive()).unwrap();
        std::fs::set_permissions(&sixth.path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads it anyway, so the test proves nothing there.
        if std::fs::File::open(&sixth.path).is_ok() {
            return;
        }
        assert_eq!(ids("since=2026-10-07T00:00:00Z").await, ["B"]);
        assert_eq!(ids("since=2026-10-08T00:00:00Z").await, ["C"]);
        // The 6th's file is read for a range touching it.
        assert_eq!(
            fixture.get("/api/messages?since=2026-10-06T00:00:00Z").await.status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// Five hits over two files, two at a time: each page carries on where the last stopped,
    /// across the files, and the last has no `next`.
    #[tokio::test]
    async fn pages_follow_next_to_the_end() {
        let (fixture, dir) = logged().await;
        let order = |minute, id| (at(8, 9, minute), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, id)]));
        let other = (at(8, 9, 3), Direction::Inbound, Some("FUNDA"), fix(&[(35, "0")]));
        log(&dir, &[order(0, "A"), order(1, "B"), other, order(4, "C")]);
        log(&dir, &[order(5, "D"), order(6, "E")]);
        let mut pages = Vec::new();
        let mut after = String::new();
        loop {
            let (ids, next) = search(&fixture, &format!("/api/messages?{DAY}&msg_type=D&limit=2{after}")).await;
            pages.push(ids);
            match next {
                Some(next) => after = format!("&after={next}"),
                None => break,
            }
        }
        assert_eq!(pages, [vec!["A", "B"], vec!["C", "D"], vec!["E"]]);
    }

    /// Each page reads up to the budget (and a record at least), and the next carries on where it
    /// stopped: with two records' bytes, a page of A and B, then the rest; with a byte, a record a
    /// page, one without a hit an empty page with a `next`. Every hit is found, once.
    #[test]
    fn the_byte_budget_cuts_pages_short() {
        let dir = tempfile::tempdir().unwrap();
        let order = |minute, id| (at(8, 9, minute), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, id)]));
        let other = (at(8, 9, 2), Direction::Inbound, Some("FUNDA"), fix(&[(35, "0")]));
        log(dir.path(), &[order(0, "A"), order(1, "B"), other, order(3, "C")]);
        let file = &FileMessageLog::files(dir.path()).unwrap()[0];
        let mut records = FileMessageLog::read(&file.path).unwrap();
        let mut positions = vec![records.position()];
        while records.next().is_some() {
            positions.push(records.position());
        }
        let pages = |bytes_max| {
            let mut filter = LogFilter::default();
            filter.msg_types = vec!["D".into()];
            let mut search = Search::new(vec![filter], at(8, 0, 0), None, None, None).unwrap();
            let mut pages = Vec::new();
            loop {
                let page = scan(dir.path(), &search, bytes_max).unwrap();
                let ids = page.messages.iter().map(|hit| hit.fields[3].1.clone()).collect::<Vec<_>>();
                search.after = page.next.map(|next| Cursor::decode(&next).unwrap());
                pages.push((ids, search.after.map(|cursor| cursor.position)));
                if search.after.is_none() {
                    return pages;
                }
            }
        };
        let two = positions[2];
        let strings = |ids: &[&str]| ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(pages(two), [(strings(&["A", "B"]), Some(two)), (strings(&["C"]), None)]);
        let ones = pages(1);
        let ids: Vec<_> = ones.iter().map(|(ids, _)| ids.clone()).collect();
        assert_eq!(ids, [vec!["A"], vec!["B"], vec![], vec!["C"], vec![]]);
        let nexts: Vec<_> = ones.iter().map(|(_, next)| *next).collect();
        assert_eq!(nexts, [Some(positions[1]), Some(positions[2]), Some(positions[3]), Some(positions[4]), None]);
    }

    /// A cursor that isn't one a search gave is refused: not base64, not a cursor, a file outside
    /// the range or not in the log, a position past its file's end or in the middle of a record.
    #[tokio::test]
    async fn a_forged_cursor_is_refused() {
        let (fixture, dir) = logged().await;
        log(&dir, &[(at(8, 9, 0), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, "A")]))]);
        let cursor = |text: &str| Base64UrlUnpadded::encode_string(text.as_bytes());
        let length = std::fs::metadata(&FileMessageLog::files(&dir).unwrap()[0].path).unwrap().len();
        let good = cursor(&format!("20261008-0-{length}"));
        let (ids, next) = search(&fixture, &format!("/api/messages?{DAY}&after={good}")).await;
        assert_eq!((ids, next), (Vec::<String>::new(), None));
        for forged in [
            "!!".to_string(),
            cursor("../../etc/passwd"),
            cursor("20261008-0"),
            cursor("20261008-0-1-2"),
            cursor("20261001-0-0"),
            cursor("20261008-7-0"),
            cursor(&format!("20261008-0-{}", length + 1)),
            cursor("20261008-0-1"),
            cursor("20261008-0--1"),
        ] {
            let answer = fixture.get(&format!("/api/messages?{DAY}&after={forged}")).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{forged}: {:?}", answer.body);
        }
    }

    /// A file being compressed, there as both `.log` and `.log.gz`, is read once; one compressed
    /// once listed is read as its `.log.gz`, from a position in the `.log`.
    #[cfg(feature = "gzip")]
    #[test]
    fn a_file_compressed_meanwhile_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let records = [
            (at(8, 9, 0), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, "A")])),
            (at(8, 9, 1), Direction::Inbound, Some("FUNDA"), fix(&[(35, "D"), (11, "B")])),
        ];
        log(dir.path(), &records);
        let [plain] = FileMessageLog::files(dir.path()).unwrap().try_into().unwrap();
        // The same records compressed, elsewhere (opening a log compresses the files there).
        let elsewhere = tempfile::tempdir().unwrap();
        log_to(elsewhere.path(), true, &records);
        let [compressed] = FileMessageLog::files(elsewhere.path()).unwrap().try_into().unwrap();
        std::fs::copy(&compressed.path, dir.path().join(compressed.path.file_name().unwrap())).unwrap();
        assert_eq!(FileMessageLog::files(dir.path()).unwrap().len(), 2);
        let search = Search::new(vec![LogFilter::default()], at(8, 0, 0), None, None, None).unwrap();
        let ids = |page: Page| page.messages.iter().map(|hit| hit.fields[3].1.clone()).collect::<Vec<_>>();
        assert_eq!(ids(scan(dir.path(), &search, SEARCH_BYTES_MAX).unwrap()), ["A", "B"]);

        let mut first = FileMessageLog::read(&plain.path).unwrap();
        first.next().unwrap().unwrap();
        std::fs::remove_file(&plain.path).unwrap();
        let mut records = open(dir.path(), &plain, first.position(), true).unwrap().unwrap();
        let LogRecord::Message { frame, .. } = records.next().unwrap().unwrap() else { panic!() };
        assert!(String::from_utf8_lossy(&frame).contains("\x0111=B\x01"));
    }

    /// Past [`MAX_SEARCHES`] under way, a search is 503; once one ends, they run again.
    #[tokio::test]
    async fn searches_past_the_cap_are_refused() {
        let (fixture, _dir) = logged().await;
        let held = fixture.state.searches.clone().try_acquire_many_owned(MAX_SEARCHES.try_into().unwrap()).unwrap();
        let answer = fixture.get(&format!("/api/messages?{DAY}")).await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
        fixture.ok(&format!("/api/messages?{DAY}")).await;
    }

    /// An order's messages, both sides': the client's with its ClOrdID in 11 or 41, the venue's
    /// with the downstream ClOrdID in 11 or 41 or the venue's OrderID in 37. Not another order's,
    /// an ID on the other side's session, or a message after the order's range.
    #[tokio::test]
    async fn an_orders_messages_on_both_sides() {
        let (fixture, dir) = logged().await;
        let order = &fixture.ok("/api/orders").await["orders"][0];
        let name = order["order_id"].as_str().unwrap().to_string();
        crate::edge::tests::acknowledge(fixture.service.gateway(), &name, "V1");
        let created: DateTime<Utc> = order["created"].as_str().unwrap().parse().unwrap();
        let at = |seconds| created + TimeDelta::seconds(seconds);
        let (client, venue) = (Some("FUNDA"), Some("NASDAQ"));
        let msg = |time, direction, comp_id, fields: &[(u32, &str)]| (time, direction, comp_id, fix(fields));
        log(
            &dir,
            &[
                msg(at(-1), Direction::Inbound, client, &[(35, "D"), (11, "C1")]),
                msg(at(0), Direction::Outbound, venue, &[(35, "D"), (11, &name)]),
                msg(at(1), Direction::Inbound, venue, &[(35, "8"), (11, "X"), (37, "V1")]),
                msg(at(2), Direction::Inbound, client, &[(35, "F"), (11, "C2"), (41, "C1")]),
                msg(at(3), Direction::Inbound, venue, &[(35, "9"), (11, "X"), (41, &name)]),
                msg(at(4), Direction::Inbound, client, &[(35, "D"), (11, "C9")]),
                msg(at(5), Direction::Inbound, venue, &[(35, "8"), (11, "C1")]),
                msg(at(6), Direction::Inbound, client, &[(35, "8"), (11, "Y"), (37, "V1")]),
            ],
        );
        // A day on, past the range.
        log(&dir, &[msg(at(86_400), Direction::Inbound, client, &[(35, "D"), (11, "C1")])]);
        let page = fixture.ok(&format!("/api/orders/{name}/messages")).await;
        let found: Vec<_> = page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| (hit["session"].as_str().unwrap().to_string(), hit["msg_type"].as_str().unwrap().to_string()))
            .collect();
        let session = |comp_id| format!("FIX.4.4:JETBRIDGE->{comp_id}");
        let expected = [
            (session("FUNDA"), "D"),
            (session("NASDAQ"), "D"),
            (session("NASDAQ"), "8"),
            (session("FUNDA"), "F"),
            (session("NASDAQ"), "9"),
        ];
        assert_eq!(found, expected.map(|(session, msg_type)| (session, msg_type.to_string())));
        assert_eq!(page["next"], serde_json::Value::Null);
        let page = fixture.ok(&format!("/api/orders/{name}/messages?limit=2")).await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 2);
        let next = page["next"].as_str().unwrap();
        let page = fixture.ok(&format!("/api/orders/{name}/messages?after={next}")).await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 3);
        assert_eq!(fixture.ok(&format!("/api/orders/{name}")).await["venue_order_id"], "V1");
    }
}
