//! The API's mutations [operator; sequence numbers, risk limits and a reload, admin]: sessions
//! paused, resumed, asked to resend and their sequence numbers set or reset, orders cancelled or
//! their venues asked where they stand, clients blocked, unblocked and cancelled, the risk limits
//! in the configuration file edited, and the configuration reloaded.
//! Each takes a JSON body with a `reason` and runs through [`audited`]: refused, and the refusal
//! recorded, for the caller's role or scope, a body that doesn't parse, or nothing to act on.
//!
//! A scoped operator or admin acts only on its CompIDs' client sessions, orders, scopes and limit
//! tables: never on a venue's session, nor on what reaches every client (`all`, the default
//! limits, a reload). A session is named as `GET /api/sessions` shows it,
//! percent-encoded as `encodeURIComponent` does: `>` as `%3E`, and a `/` (before a SubID or
//! LocationID) as `%2F`, so it stays one segment of the path.

use std::sync::Arc;

use axum::Json;
use axum::extract::Path;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use turbojet::{SequenceError, SequenceNumbers, SessionHandle};
use turbojet_config::Changes;

use super::auth::{Caller, Target};
use super::{ApiError, Mutation, Outcome, State, audited};
use crate::auth::Role;
use crate::book::{OrderId, Scope, ScopeName};
use crate::config::RiskTable;
use crate::edge::{CancelError, Gateway, StatusError};
use crate::limits::{self, Edit, EditError, Limits};
use crate::service::{ConfigSummary, Edited, Reloaded, Service};

type AxumState = axum::extract::State<State>;
type Body<B> = Result<Json<B>, JsonRejection>;
type Id = Result<Path<String>, PathRejection>;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reason {
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Resend {
    reason: Option<String>,
    /// The first MsgSeqNum to ask for again.
    from: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Sequence {
    reason: Option<String>,
    next_incoming: Option<u64>,
    next_outgoing: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScopeBody {
    reason: Option<String>,
    /// `all`, `comp:COMPID` or `session:SESSION`.
    scope: Option<String>,
}

/// `POST /api/sessions/{id}/pause {reason}`: logs the session out and keeps it from logging on
/// until resumed.
pub(super) async fn pause(
    axum::extract::State(state): AxumState,
    caller: Caller,
    id: Id,
    body: Body<Reason>,
) -> Result<StatusCode, ApiError> {
    let (id, parsed_id) = parse_id(id);
    let (body, parsed) = parse(body);
    let parsed = parsed_id.and(parsed);
    let mutation = Mutation { action: "pause", target: id, params: serde_json::Value::Null, reason: body.reason };
    on_session(&state, &caller, mutation, parsed, |session, ()| async move {
        // Best effort: another pause or resume may come between the check and the act.
        let note = session.is_paused().then(|| "unchanged: already paused".to_string());
        // The reason is the operator's, not the counterparty's to read.
        session.pause(None);
        Ok((StatusCode::NO_CONTENT, note))
    })
    .await
}

/// `POST /api/sessions/{id}/resume {reason}`: lets a paused session log on again.
pub(super) async fn resume(
    axum::extract::State(state): AxumState,
    caller: Caller,
    id: Id,
    body: Body<Reason>,
) -> Result<StatusCode, ApiError> {
    let (id, parsed_id) = parse_id(id);
    let (body, parsed) = parse(body);
    let parsed = parsed_id.and(parsed);
    let mutation = Mutation { action: "resume", target: id, params: serde_json::Value::Null, reason: body.reason };
    on_session(&state, &caller, mutation, parsed, |session, ()| async move {
        // Best effort, as pause's.
        let note = (!session.is_paused()).then(|| "unchanged: not paused".to_string());
        session.resume();
        Ok((StatusCode::NO_CONTENT, note))
    })
    .await
}

/// The session's sequence numbers after a resend request or a sequence change.
#[derive(Debug, Serialize)]
pub(super) struct Numbers {
    next_incoming: u64,
    next_outgoing: u64,
}

/// `POST /api/sessions/{id}/resend {reason, from}`: asks the counterparty to send again from
/// MsgSeqNum `from`; see [`SessionHandle::request_resend`]. 409 if the session isn't logged on, a
/// resend is under way, or `from` isn't below the next expected.
pub(super) async fn resend(
    axum::extract::State(state): AxumState,
    caller: Caller,
    id: Id,
    body: Body<Resend>,
) -> Result<Json<Numbers>, ApiError> {
    let (id, parsed_id) = parse_id(id);
    let (body, parsed) = parse(body);
    let parsed = parsed_id.and(parsed);
    let from = body.from;
    let parsed =
        parsed.and_then(|()| from.ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "a resend needs from")));
    let params = serde_json::json!({ "from": from });
    let mutation = Mutation { action: "resend", target: id, params, reason: body.reason };
    on_session(&state, &caller, mutation, parsed, |session, from| async move {
        Ok((Json(numbers(session.request_resend(from).await)?), None))
    })
    .await
}

/// `POST /api/sessions/{id}/sequence {reason, next_incoming, next_outgoing}` [admin]: sets either
/// number or both; see [`SessionHandle::set_next_incoming`] and
/// [`SessionHandle::set_next_outgoing`]. 400 for 0, 409 for an outgoing number below the next.
///
/// Lowering next_incoming on a logged-on session can have messages accepted again, or, if the
/// counterparty has messages in flight, a ResendRequest or a Logout; pausing it first is safer.
///
/// Outgoing goes first, as the one refused for going back, so a refusal leaves nothing changed.
/// Only a store failure setting incoming then leaves outgoing set, and the error (and the audit
/// line) says so.
pub(super) async fn sequence(
    axum::extract::State(state): AxumState,
    caller: Caller,
    id: Id,
    body: Body<Sequence>,
) -> Result<Json<Numbers>, ApiError> {
    let (id, parsed_id) = parse_id(id);
    let (body, parsed) = parse(body);
    let parsed = caller.require(Role::Admin).and(parsed_id).and(parsed);
    let (incoming, outgoing) = (body.next_incoming, body.next_outgoing);
    let parsed = parsed.and_then(|()| match (incoming, outgoing) {
        (None, None) => Err(ApiError::new(StatusCode::BAD_REQUEST, "needs next_incoming, next_outgoing or both")),
        (Some(0), _) | (_, Some(0)) => Err(ApiError::new(StatusCode::BAD_REQUEST, "sequence numbers start at 1")),
        numbers => Ok(numbers),
    });
    let params = serde_json::json!({ "next_incoming": incoming, "next_outgoing": outgoing });
    let mutation = Mutation { action: "sequence", target: id, params, reason: body.reason };
    on_session(&state, &caller, mutation, parsed, |session, (incoming, outgoing)| async move {
        let set = match (incoming, outgoing) {
            (None, Some(outgoing)) => numbers(session.set_next_outgoing(outgoing).await)?,
            (Some(incoming), None) => numbers(session.set_next_incoming(incoming).await)?,
            (Some(incoming), Some(outgoing)) => {
                numbers(session.set_next_outgoing(outgoing).await)?;
                numbers(session.set_next_incoming(incoming).await).map_err(|mut e| {
                    e.message = format!("next_outgoing is set to {outgoing}, but next_incoming isn't: {}", e.message);
                    e
                })?
            }
            (None, None) => unreachable!("refused by parsed"),
        };
        Ok((Json(set), None))
    })
    .await
}

/// `POST /api/sessions/{id}/reset {reason}` [admin]: resets both sequence numbers to 1 and clears
/// the resend store; see [`SessionHandle::reset_sequence_numbers`]. 409 while connected.
pub(super) async fn reset(
    axum::extract::State(state): AxumState,
    caller: Caller,
    id: Id,
    body: Body<Reason>,
) -> Result<Json<Numbers>, ApiError> {
    let (id, parsed_id) = parse_id(id);
    let (body, parsed) = parse(body);
    let parsed = caller.require(Role::Admin).and(parsed_id).and(parsed);
    let params = serde_json::json!({ "next_incoming": 1, "next_outgoing": 1 });
    let mutation = Mutation { action: "reset", target: id, params, reason: body.reason };
    on_session(&state, &caller, mutation, parsed, |session, ()| async move {
        Ok((Json(numbers(session.reset_sequence_numbers().await)?), None))
    })
    .await
}

/// A sequence change's numbers, or 409 for a change refused and 500 for the store failing.
fn numbers(result: Result<SequenceNumbers, SequenceError>) -> Result<Numbers, ApiError> {
    match result {
        Ok(numbers) => Ok(Numbers { next_incoming: numbers.next_incoming, next_outgoing: numbers.next_outgoing }),
        Err(SequenceError::Storage(e)) => {
            Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("the session store failed: {e}")))
        }
        Err(e) => Err(ApiError::new(StatusCode::CONFLICT, e.to_string())),
    }
}

/// `POST /api/orders/{order_id}/cancel {reason}`: the gateway's own cancel of the order whose
/// OrderID(37) is `order_id` (see [`Gateway::cancel_order`]); 404 if the book doesn't hold it,
/// 409 if it's done.
pub(super) async fn cancel_order(
    axum::extract::State(state): AxumState,
    caller: Caller,
    order_id: Id,
    body: Body<Reason>,
) -> Result<StatusCode, ApiError> {
    let (order_id, parsed_id) = parse_id(order_id);
    let (body, parsed) = parse(body);
    let gateway = state.service()?.gateway().clone();
    let found = caller
        .require(Role::Operator)
        .and(parsed_id)
        .and(parsed)
        .and_then(|()| find_order(&caller, &gateway, order_id.as_deref()));
    let unknown = format!("no order {}", order_id.as_deref().unwrap_or_default());
    let mutation =
        Mutation { action: "cancel_order", target: order_id, params: serde_json::Value::Null, reason: body.reason };
    audited_on(&state, &caller, mutation, found, |order| async move {
        gateway.cancel_order(order).await.map_err(|e| match e {
            CancelError::UnknownOrder => ApiError::new(StatusCode::NOT_FOUND, unknown),
            CancelError::Done => ApiError::new(StatusCode::CONFLICT, e.to_string()),
        })?;
        Ok((StatusCode::NO_CONTENT, None))
    })
    .await
}

/// `POST /api/orders/{order_id}/status {reason}`: asks the venue of the order whose OrderID(37)
/// is `order_id` where it stands (see [`Gateway::request_status`]); 202 once sent, as the answer
/// comes later, if at all, as the order's reports. 404 if the book doesn't hold the order, 409 if
/// it's done or its venue isn't logged on.
pub(super) async fn request_status(
    axum::extract::State(state): AxumState,
    caller: Caller,
    order_id: Id,
    body: Body<Reason>,
) -> Result<StatusCode, ApiError> {
    let (order_id, parsed_id) = parse_id(order_id);
    let (body, parsed) = parse(body);
    let gateway = state.service()?.gateway().clone();
    let found = caller
        .require(Role::Operator)
        .and(parsed_id)
        .and(parsed)
        .and_then(|()| find_order(&caller, &gateway, order_id.as_deref()));
    let unknown = format!("no order {}", order_id.as_deref().unwrap_or_default());
    let mutation =
        Mutation { action: "request_status", target: order_id, params: serde_json::Value::Null, reason: body.reason };
    audited_on(&state, &caller, mutation, found, |order| async move {
        gateway.request_status(order).map_err(|e| match e {
            StatusError::UnknownOrder => ApiError::new(StatusCode::NOT_FOUND, unknown),
            StatusError::Done | StatusError::VenueUnavailable => ApiError::new(StatusCode::CONFLICT, e.to_string()),
        })?;
        Ok((StatusCode::ACCEPTED, None))
    })
    .await
}

/// The order whose OrderID(37) is `order_id` (`Some`, once parsed), if the book holds it and the
/// caller may act on its client.
fn find_order(caller: &Caller, gateway: &Gateway, order_id: Option<&str>) -> Result<OrderId, ApiError> {
    let order_id = order_id.expect("guarded by parse_id");
    let order = gateway.order_id(order_id).and_then(|id| gateway.order(id));
    let order = order.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no order {order_id}")))?;
    if caller.may_act_on(&order.order.client) { Ok(order.order.id) } else { Err(out_of_scope(caller, order_id)) }
}

/// Whether a block or unblock changed anything.
#[derive(Debug, Serialize)]
pub(super) struct Changed {
    changed: bool,
}

/// `POST /api/risk/block {reason, scope}`: rejects the new orders of the clients in `scope`; see
/// [`Gateway::block`].
pub(super) async fn block(
    axum::extract::State(state): AxumState,
    caller: Caller,
    body: Body<ScopeBody>,
) -> Result<Json<Changed>, ApiError> {
    on_scope(&state, &caller, "block", body, |gateway, scope| async move {
        let changed = gateway.block(scope).await;
        Ok((Json(Changed { changed }), (!changed).then(|| "unchanged: already blocked".to_string())))
    })
    .await
}

/// `POST /api/risk/unblock {reason, scope}`: lifts the block of exactly `scope`; see
/// [`Gateway::unblock`].
pub(super) async fn unblock(
    axum::extract::State(state): AxumState,
    caller: Caller,
    body: Body<ScopeBody>,
) -> Result<Json<Changed>, ApiError> {
    on_scope(&state, &caller, "unblock", body, |gateway, scope| async move {
        let changed = gateway.unblock(scope).await;
        Ok((Json(Changed { changed }), (!changed).then(|| "unchanged: not blocked".to_string())))
    })
    .await
}

/// `POST /api/risk/cancel-all {reason, scope}`: the kill switch, blocking the clients in `scope`
/// and cancelling their orders; see [`Gateway::cancel_all`].
pub(super) async fn cancel_all(
    axum::extract::State(state): AxumState,
    caller: Caller,
    body: Body<ScopeBody>,
) -> Result<StatusCode, ApiError> {
    on_scope(&state, &caller, "cancel_all", body, |gateway, scope| async move {
        gateway.cancel_all(scope).await;
        Ok((StatusCode::NO_CONTENT, None))
    })
    .await
}

/// What a reload took.
#[derive(Debug, Serialize)]
pub(super) struct ReloadView {
    sessions: SessionChanges,
    /// The routes, limits and `day_end`.
    config: ConfigSummary,
    warnings: Vec<String>,
}

/// [`Changes`]: counterparties by CompID, initiators by section name.
#[derive(Debug, Serialize)]
struct SessionChanges {
    added: Vec<String>,
    changed: Vec<String>,
    removed: Vec<String>,
    logged_out: Vec<String>,
    started: Vec<String>,
    reconfigured: Vec<String>,
    stopped: Vec<String>,
}

impl From<Changes> for SessionChanges {
    fn from(changes: Changes) -> Self {
        let Changes { added, changed, removed, logged_out, started, reconfigured, stopped } = changes;
        Self { added, changed, removed, logged_out, started, reconfigured, stopped }
    }
}

/// `POST /api/reload {reason}` [unscoped admin]: rereads the files, as SIGHUP does (see
/// [`Service::reload`]), answering what changed; 422 with why if a file was kept.
pub(super) async fn reload(
    axum::extract::State(state): AxumState,
    caller: Caller,
    body: Body<Reason>,
) -> Result<Json<ReloadView>, ApiError> {
    let (body, parsed) = parse(body);
    let service = state.service()?;
    let allowed = caller.require(Role::Admin).and(parsed).and_then(|()| unscoped(&caller, "a reload"));
    let mutation = Mutation { action: "reload", target: None, params: serde_json::Value::Null, reason: body.reason };
    let reload = async move {
        let reloaded = tokio::task::spawn_blocking(move || service.reload())
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("the reload failed: {e}")))?
            .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the gateway is stopping"))?;
        answer(reloaded)
    };
    audited(&state, &caller, mutation, allowed, reload).await
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LimitsBody {
    reason: Option<String>,
    /// Each key's new value, or `null` to remove it.
    set: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What a limit edit answers: the table edited as the file now has it (`null` once deleted), and
/// what the reload took.
#[derive(Debug, Serialize)]
pub(super) struct EditedView {
    table: Option<RiskTable>,
    reload: ReloadView,
}

/// `PATCH /api/risk/limits/defaults {reason, set: {key: value | null}}` [unscoped admin]: sets
/// the defaults' keys, removing those set to `null`, in `jetbridge.toml`, and reloads; see
/// [`Service::edit_limits`]. The keys and their values are the file's: an amount is a JSON number
/// or a decimal as a string (which keeps every digit), `symbols` an array of strings,
/// `self_trade` a boolean; 400 for another key or type. 422 with why if the file wouldn't load
/// with the edit, or doesn't load now: it's left as it was.
///
/// The reload takes whatever else changed in the files since the last, as any reload does. A
/// reload that keeps something answers 422 with why, though the edit is written.
pub(super) async fn set_default_limits(
    axum::extract::State(state): AxumState,
    caller: Caller,
    body: Body<LimitsBody>,
) -> Result<Json<EditedView>, ApiError> {
    let (body, parsed) = parse(body);
    let allowed = caller.require(Role::Admin).and(parsed).and_then(|()| unscoped(&caller, "risk.defaults"));
    set_limits(&state, &caller, Limits::Defaults, "risk.defaults".to_string(), allowed, body).await
}

/// `PATCH /api/risk/limits/client/{comp_id} {reason, set: {key: value | null}}` [admin of the
/// CompID]: as [`set_default_limits`], for the CompID's own table, created if it has none. A key
/// it removes is the defaults' again.
pub(super) async fn set_client_limits(
    axum::extract::State(state): AxumState,
    caller: Caller,
    comp_id: Id,
    body: Body<LimitsBody>,
) -> Result<Json<EditedView>, ApiError> {
    let (comp_id, parsed_id) = parse_id(comp_id);
    let (body, parsed) = parse(body);
    let comp_id = comp_id.unwrap_or_default();
    let allowed = caller
        .require(Role::Admin)
        .and(parsed_id)
        .and(parsed)
        .and_then(|()| limits::check_comp_id(&comp_id).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e)))
        .and_then(|()| in_scope(&caller, &comp_id));
    let target = format!("risk.client.{comp_id}");
    set_limits(&state, &caller, Limits::Client(comp_id), target, allowed, body).await
}

/// `DELETE /api/risk/limits/client/{comp_id} {reason}` [admin of the CompID]: removes the
/// CompID's table from `jetbridge.toml`, so its sessions get the defaults, and reloads, as
/// [`set_default_limits`] does; 404 if it has none. A PATCH removing every key leaves the table, empty: the client gets
/// the defaults but still has a table, until this removes it.
pub(super) async fn delete_client_limits(
    axum::extract::State(state): AxumState,
    caller: Caller,
    comp_id: Id,
    body: Body<Reason>,
) -> Result<Json<EditedView>, ApiError> {
    let (comp_id, parsed_id) = parse_id(comp_id);
    let (body, parsed) = parse(body);
    let comp_id = comp_id.unwrap_or_default();
    let edit = caller
        .require(Role::Admin)
        .and(parsed_id)
        .and(parsed)
        .and_then(|()| limits::check_comp_id(&comp_id).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e)))
        .and_then(|()| in_scope(&caller, &comp_id))
        .map(|()| Edit::Delete { comp_id: comp_id.clone() });
    let target = Some(Limits::Client(comp_id.clone()).to_string());
    let mutation = Mutation { action: "delete_limits", target, params: serde_json::Value::Null, reason: body.reason };
    edit_limits(&state, &caller, mutation, edit).await
}

/// A set of `body`'s keys in `table`, once `allowed`; audited with the keys set, and their values,
/// and those removed.
async fn set_limits(
    state: &State,
    caller: &Caller,
    table: Limits,
    target: String,
    allowed: Result<(), ApiError>,
    body: LimitsBody,
) -> Result<Json<EditedView>, ApiError> {
    let set = body.set.unwrap_or_default();
    let edit = allowed.and_then(|()| Edit::set(table, &set).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e)));
    let (removed, set): (Vec<_>, Vec<_>) = set.into_iter().partition(|(_, value)| value.is_null());
    let removed: Vec<_> = removed.into_iter().map(|(key, _)| key).collect();
    let params = serde_json::json!({ "set": serde_json::Map::from_iter(set), "removed": removed });
    let mutation = Mutation { action: "set_limits", target: Some(target), params, reason: body.reason };
    edit_limits(state, caller, mutation, edit).await
}

/// Runs `edit`, audited as `mutation`, if it parsed; see [`Service::edit_limits`].
async fn edit_limits(
    state: &State,
    caller: &Caller,
    mutation: Mutation,
    edit: Result<Edit, ApiError>,
) -> Result<Json<EditedView>, ApiError> {
    let service = state.service()?;
    audited_on(state, caller, mutation, edit, |edit| async move {
        let edited = tokio::task::spawn_blocking(move || service.edit_limits(&edit))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("the edit failed: {e}")))?;
        let Edited { table, reloaded } = edited.map_err(|e| {
            let status = match e {
                EditError::Stopping => StatusCode::SERVICE_UNAVAILABLE,
                EditError::NoTable(_) => StatusCode::NOT_FOUND,
                EditError::Refused(_) => StatusCode::UNPROCESSABLE_ENTITY,
                EditError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            ApiError::new(status, e.to_string())
        })?;
        let (Json(reload), note) = answer(reloaded).map_err(|mut e| {
            e.message = format!("the edit is written, but the reload {}", e.message);
            e
        })?;
        Ok((Json(EditedView { table, reload }), note))
    })
    .await
}

/// A reload's answer: all of it taken, or 422 (see [`partly_kept`]).
fn answer(reloaded: Reloaded) -> Outcome<Json<ReloadView>> {
    let Reloaded { sessions, config, warnings } = reloaded;
    match (sessions, config) {
        (Ok(changes), Ok(config)) => Ok((Json(ReloadView { sessions: changes.into(), config, warnings }), None)),
        (sessions, config) => Err(partly_kept(sessions, config, warnings)),
    }
}

/// 422 saying which file was kept and why, what the other half took, and the warnings.
fn partly_kept(
    sessions: Result<Changes, String>,
    config: Result<ConfigSummary, String>,
    warnings: Vec<String>,
) -> ApiError {
    let mut why = Vec::new();
    match sessions {
        Ok(changes) => why.push(format!("took the sessions file ({})", changed(changes))),
        Err(e) => why.push(format!("kept the sessions file: {e}")),
    }
    match config {
        Ok(_) => why.push("took the routes, limits and day_end".to_string()),
        Err(e) => why.push(format!("kept the routes, limits and day_end: {e}")),
    }
    why.extend(warnings);
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, why.join("; "))
}

/// What a sessions file taken changed, for a 422's message and so the audit line: `added FUNDB;
/// stopped LSE`, or `nothing changed`.
fn changed(changes: Changes) -> String {
    let SessionChanges { added, changed, removed, logged_out, started, reconfigured, stopped } = changes.into();
    let lists = [
        ("added", added),
        ("changed", changed),
        ("removed", removed),
        ("logged out", logged_out),
        ("started", started),
        ("reconfigured", reconfigured),
        ("stopped", stopped),
    ];
    let lists: Vec<_> = lists
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(what, names)| format!("{what} {}", names.join(", ")))
        .collect();
    if lists.is_empty() { "nothing changed".to_string() } else { lists.join("; ") }
}

/// The path's ID, or why it didn't decode, refused as a body that doesn't parse is.
fn parse_id(id: Id) -> (Option<String>, Result<(), ApiError>) {
    match id {
        Ok(Path(id)) => (Some(id), Ok(())),
        Err(e) => (None, Err(ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))),
    }
}

/// The body, or its default and why it's refused (400).
fn parse<B: Default>(body: Body<B>) -> (B, Result<(), ApiError>) {
    match body {
        Ok(Json(body)) => (body, Ok(())),
        Err(e) => (B::default(), Err(ApiError::new(StatusCode::BAD_REQUEST, e.body_text()))),
    }
}

/// Runs `action` on what `found` holds, as [`audited`] does; or refuses with why it holds nothing.
async fn audited_on<F, T, A>(
    state: &State,
    caller: &Caller,
    mutation: Mutation,
    found: Result<F, ApiError>,
    action: impl FnOnce(F) -> A,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    A: Future<Output = Outcome<T>> + Send + 'static,
{
    match found {
        Ok(found) => audited(state, caller, mutation, Ok(()), action(found)).await,
        Err(refused) => audited(state, caller, mutation, Err(refused.clone()), std::future::ready(Err(refused))).await,
    }
}

/// A session mutation [operator; an admin's if `parsed` requires it]: `action` on the session
/// `mutation.target` names, with what `parsed` (the body, checked) holds, once the caller may act
/// on it.
async fn on_session<P, T, A>(
    state: &State,
    caller: &Caller,
    mutation: Mutation,
    parsed: Result<P, ApiError>,
    action: impl FnOnce(SessionHandle, P) -> A,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    A: Future<Output = Outcome<T>> + Send + 'static,
{
    let service = state.service()?;
    let found = caller.require(Role::Operator).and(parsed).and_then(|parsed| {
        let id = mutation.target.as_deref().expect("a parsed session mutation names its session");
        let session = find_session(&service, id)?;
        if caller.may_act_on(&session) { Ok((session.handle(), parsed)) } else { Err(out_of_scope(caller, id)) }
    });
    audited_on(state, caller, mutation, found, |(session, parsed)| action(session, parsed)).await
}

/// A session as the mutations find it.
enum Session {
    /// A client's: a scoped operator's if it's one of their CompIDs.
    Client(SessionHandle),
    /// A venue's: an unscoped operator's only.
    Venue(SessionHandle),
}

impl Session {
    fn handle(self) -> SessionHandle {
        match self {
            Self::Client(handle) | Self::Venue(handle) => handle,
        }
    }
}

impl Target for Session {
    fn comp_id(&self) -> Option<&str> {
        match self {
            Self::Client(handle) => Some(&handle.id().target_comp_id),
            Self::Venue(_) => None,
        }
    }
}

/// The session that shows as `id`: a running venue's, or a client's connected or paused; 404
/// otherwise.
fn find_session(service: &Service, id: &str) -> Result<Session, ApiError> {
    let mut venues = service.venues().into_iter().filter_map(|name| service.venue_session(&name));
    if let Some(venue) = venues.find(|handle| handle.id().to_string() == id) {
        return Ok(Session::Venue(venue));
    }
    let mut clients = service.client_statuses().into_iter().map(|status| status.id).chain(service.client_paused());
    let client = clients.find(|client| client.to_string() == id);
    let client = client.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no session {id}")))?;
    Ok(Session::Client(service.client_session(&client)))
}

/// A scope mutation [operator]: `action` on the scope the body names, once the caller may act on
/// it. A session scope names a session the gateway has seen; 404 otherwise.
async fn on_scope<T, A>(
    state: &State,
    caller: &Caller,
    action_name: &'static str,
    body: Body<ScopeBody>,
    action: impl FnOnce(Arc<Gateway>, Scope) -> A,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    A: Future<Output = Outcome<T>> + Send + 'static,
{
    let (body, parsed) = parse(body);
    let gateway = state.service()?.gateway().clone();
    let found = caller.require(Role::Operator).and(parsed).and_then(|()| {
        let text = body.scope.as_deref().ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "needs a scope: all, comp:COMPID or session:SESSION")
        })?;
        let name: ScopeName = text.parse().map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("{e}")))?;
        let scope = gateway.scope(&name).ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no {name}")))?;
        if caller.may_act_on(&scope) { Ok(scope) } else { Err(out_of_scope(caller, text)) }
    });
    let mutation =
        Mutation { action: action_name, target: body.scope, params: serde_json::Value::Null, reason: body.reason };
    audited_on(state, caller, mutation, found, |scope| action(gateway, scope)).await
}

/// 403 for a scoped caller acting on another CompID's client.
fn in_scope(caller: &Caller, comp_id: &str) -> Result<(), ApiError> {
    if caller.may_act_on(comp_id) { Ok(()) } else { Err(out_of_scope(caller, comp_id)) }
}

/// 403 for a scoped caller acting on what affects every client, as `all` does: `what`.
fn unscoped(caller: &Caller, what: &str) -> Result<(), ApiError> {
    if caller.may_act_on(&Scope::All) { Ok(()) } else { Err(out_of_scope(caller, what)) }
}

/// 403 for a scoped caller acting on what isn't theirs.
fn out_of_scope(caller: &Caller, target: &str) -> ApiError {
    let clients = caller.clients.as_deref().unwrap_or_default().join(", ");
    ApiError::new(StatusCode::FORBIDDEN, format!("{} may act only on clients {clients}, not {target}", caller.user))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use turbojet::fields::Decimal;

    use super::super::read::tests::{Fixture, fixture};
    use super::super::tests::{Answer, send};
    use super::*;
    use crate::book::{ExecType, Input, OrderId, Status, VenueReport};

    /// FUNDA's session, as a path segment.
    const FUNDA: &str = "FIX.4.4:JETBRIDGE-%3EFUNDA";
    const REASON: &str = r#"{"reason":"testing"}"#;

    impl Fixture {
        /// `POST path` with `body`, as `bearer` (none if empty).
        async fn post(&self, bearer: &str, path: &str, body: &str) -> Answer {
            self.send("POST", bearer, path, body).await
        }

        /// `method path` with `body`, as `bearer` (none if empty).
        async fn send(&self, method: &str, bearer: &str, path: &str, body: &str) -> Answer {
            let headers: &[(&str, &str)] = if bearer.is_empty() { &[] } else { &[("authorization", bearer)] };
            send(&self.app, method, path, headers, body).await
        }

        /// `method path` with `body` as the admin, which must answer `status`: its body.
        async fn admin(&self, method: &str, path: &str, body: &str, status: StatusCode) -> serde_json::Value {
            let answer = self.send(method, &self.admin, path, body).await;
            assert_eq!(answer.status, status, "{method} {path} {body}: {}", answer.body);
            answer.body
        }

        /// The configuration file's text.
        fn config(&self) -> String {
            std::fs::read_to_string(self.dirs.0.path().join("jetbridge.toml")).unwrap()
        }

        /// `POST path` with `body` as the operator, which must answer `status`: its body.
        async fn expect(&self, path: &str, body: &str, status: StatusCode) -> serde_json::Value {
            let answer = self.post(&self.operator, path, body).await;
            assert_eq!(answer.status, status, "{path} {body}: {}", answer.body);
            answer.body
        }

        /// The venue's session, as a path segment.
        async fn venue(&self) -> String {
            let sessions = self.ok("/api/sessions").await;
            let venue = sessions.as_array().unwrap().iter().find(|s| s["side"] == "venue").unwrap();
            venue["id"].as_str().unwrap().replace('>', "%3E")
        }

        /// Order 1's OrderID(37).
        async fn order_name(&self) -> String {
            self.ok("/api/orders").await["orders"][0]["order_id"].as_str().unwrap().to_string()
        }

        /// The audit log's results, in order.
        fn results(&self) -> Vec<String> {
            let text = std::fs::read_to_string(self.dirs.1.path().join("audit.jsonl")).unwrap_or_default();
            let lines = text.lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
            lines.map(|line| line["result"].as_str().unwrap().to_string()).collect()
        }

        fn last_result(&self) -> String {
            self.results().pop().unwrap()
        }

        /// Waits until FUNDA is logged on, or (`false`) disconnected.
        async fn wait_for_funda(&self, logged_on: bool) {
            let is = || {
                let statuses = self.service.client_statuses();
                if !logged_on {
                    return statuses.is_empty();
                }
                statuses
                    .iter()
                    .any(|s| s.activity.as_ref().is_some_and(|a| a.state == turbojet::SessionState::LoggedOn))
            };
            for _ in 0..500 {
                if is() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("FUNDA didn't get to logged on {logged_on}");
        }
    }

    /// Every endpoint as each role calls it: no one (401), a viewer (403), an operator, an
    /// operator scoped to FUNDB (403: FUNDA's session, order and CompID, and the venue), and an
    /// admin; sequence numbers and a reload are an admin's only. An allowed call answers success,
    /// or on a row marked so, 409 (a second resend while the first is under way). Pausing FUNDA
    /// comes last, as it logs FUNDA out.
    #[tokio::test]
    async fn every_endpoint_by_role() {
        let fixture = fixture().await;
        let (venue, order) = (fixture.venue().await, fixture.order_name().await);
        let (none, forbidden) = (Some(StatusCode::UNAUTHORIZED), Some(StatusCode::FORBIDDEN));
        let operator = [none, forbidden, None, forbidden, None];
        let admin = [none, forbidden, forbidden, forbidden, None];
        let funda = r#"{"reason":"testing","scope":"comp:FUNDA"}"#;
        let rows = [
            ("POST", format!("/api/sessions/{FUNDA}/resend"), r#"{"reason":"testing","from":1}"#, operator, true),
            ("POST", format!("/api/orders/{order}/cancel"), REASON, operator, false),
            // The venue never logs on.
            ("POST", format!("/api/orders/{order}/status"), REASON, operator, true),
            ("POST", "/api/risk/block".to_string(), funda, operator, false),
            ("POST", "/api/risk/unblock".to_string(), funda, operator, false),
            ("POST", "/api/risk/cancel-all".to_string(), funda, operator, false),
            ("POST", format!("/api/sessions/{venue}/pause"), REASON, operator, false),
            ("POST", format!("/api/sessions/{venue}/resume"), REASON, operator, false),
            (
                "POST",
                format!("/api/sessions/{venue}/sequence"),
                r#"{"reason":"testing","next_incoming":1}"#,
                admin,
                false,
            ),
            ("POST", format!("/api/sessions/{venue}/reset"), REASON, admin, false),
            (
                "PATCH",
                "/api/risk/limits/defaults".to_string(),
                r#"{"reason":"testing","set":{"credit":5}}"#,
                admin,
                false,
            ),
            (
                "PATCH",
                "/api/risk/limits/client/FUNDB".to_string(),
                r#"{"reason":"testing","set":{"credit":5}}"#,
                admin,
                false,
            ),
            ("DELETE", "/api/risk/limits/client/FUNDB".to_string(), REASON, admin, false),
            ("POST", "/api/reload".to_string(), REASON, admin, false),
            ("POST", format!("/api/sessions/{FUNDA}/pause"), REASON, operator, false),
        ];
        let callers = [
            ("no one", ""),
            ("viewer", fixture.viewer.as_str()),
            ("operator", &fixture.operator),
            ("scoped", &fixture.scoped),
            ("admin", &fixture.admin),
        ];
        for (method, path, body, wants, conflict) in rows {
            for ((caller, bearer), want) in callers.iter().zip(wants) {
                let answer = fixture.send(method, bearer, &path, body).await;
                match want {
                    Some(want) => assert_eq!(answer.status, want, "{path} as {caller}: {}", answer.body),
                    None => assert!(
                        answer.status.is_success() || (conflict && answer.status == StatusCode::CONFLICT),
                        "{path} as {caller}: {} {}",
                        answer.status,
                        answer.body
                    ),
                }
            }
        }
    }

    /// A resend asks from `from`; pausing logs FUNDA out and lists it paused, resuming lets it
    /// back; a venue's session pauses too. Doing either twice is recorded as unchanged.
    #[tokio::test]
    async fn sessions_resend_pause_and_resume() {
        let fixture = fixture().await;
        let resend = format!("/api/sessions/{FUNDA}/resend");
        fixture.expect(&resend, REASON, StatusCode::BAD_REQUEST).await;
        assert_eq!(fixture.last_result(), "refused: a resend needs from");
        let numbers = fixture.expect(&resend, r#"{"reason":"testing","from":1}"#, StatusCode::OK).await;
        assert_eq!(numbers["next_incoming"], 1, "{numbers}");

        let pause = format!("/api/sessions/{FUNDA}/pause");
        fixture.expect(&pause, REASON, StatusCode::NO_CONTENT).await;
        fixture.wait_for_funda(false).await;
        let sessions = fixture.ok("/api/sessions").await;
        assert_eq!((&sessions[0]["status"], &sessions[0]["paused"]), (&"disconnected".into(), &true.into()));
        fixture.expect(&pause, REASON, StatusCode::NO_CONTENT).await;
        assert_eq!(fixture.last_result(), "unchanged: already paused");

        let resume = format!("/api/sessions/{FUNDA}/resume");
        fixture.expect(&resume, REASON, StatusCode::NO_CONTENT).await;
        fixture.wait_for_funda(true).await;
        fixture.expect(&resume, REASON, StatusCode::NO_CONTENT).await;
        assert_eq!(fixture.last_result(), "unchanged: not paused");

        let venue = fixture.venue().await;
        let nasdaq = || fixture.service.venue_session("NASDAQ").unwrap();
        fixture.expect(&format!("/api/sessions/{venue}/pause"), REASON, StatusCode::NO_CONTENT).await;
        assert!(nasdaq().is_paused());
        fixture.expect(&format!("/api/sessions/{venue}/resume"), REASON, StatusCode::NO_CONTENT).await;
        assert!(!nasdaq().is_paused());
        // Not logged on.
        fixture
            .expect(&format!("/api/sessions/{venue}/resend"), r#"{"reason":"r","from":1}"#, StatusCode::CONFLICT)
            .await;

        // A `/` in the name stays in its segment.
        let body = fixture.expect(&format!("/api/sessions/{FUNDA}%2FDESK/pause"), REASON, StatusCode::NOT_FOUND).await;
        assert_eq!(body["error"], "no session FIX.4.4:JETBRIDGE->FUNDA/DESK");
    }

    /// An admin sets FUNDA's outgoing number while it's logged on, but not below the next, and
    /// can't reset it then; a 0 for either is refused before anything is set. Once FUNDA is
    /// paused, it resets to 1 and takes an incoming number; the venue, never logged on, takes
    /// both. Each is audited with the numbers asked for.
    #[tokio::test]
    async fn sessions_sequence_and_reset() {
        let fixture = fixture().await;
        let post = async |path: &str, body: &str, status: StatusCode| {
            let answer = fixture.post(&fixture.admin, path, body).await;
            assert_eq!(answer.status, status, "{path} {body}: {}", answer.body);
            answer.body
        };
        let numbers = |body: &serde_json::Value| (body["next_incoming"].as_u64(), body["next_outgoing"].as_u64());
        let last_params = || {
            let audit = std::fs::read_to_string(fixture.dirs.1.path().join("audit.jsonl")).unwrap();
            serde_json::from_str::<serde_json::Value>(audit.lines().last().unwrap()).unwrap()["params"].clone()
        };
        let (sequence, reset) = (format!("/api/sessions/{FUNDA}/sequence"), format!("/api/sessions/{FUNDA}/reset"));
        post(&sequence, REASON, StatusCode::BAD_REQUEST).await;
        assert_eq!(fixture.last_result(), "refused: needs next_incoming, next_outgoing or both");
        let body = post(&sequence, r#"{"reason":"r","next_outgoing":100}"#, StatusCode::OK).await;
        assert_eq!(body["next_outgoing"], 100, "{body}");
        post(&sequence, r#"{"reason":"r","next_outgoing":50}"#, StatusCode::CONFLICT).await;
        post(&sequence, r#"{"reason":"r","next_incoming":0,"next_outgoing":200}"#, StatusCode::BAD_REQUEST).await;
        assert_eq!(fixture.last_result(), "refused: sequence numbers start at 1");
        assert_eq!(fixture.ok("/api/sessions").await[0]["next_outgoing"], 100);
        assert_eq!(last_params(), serde_json::json!({ "next_incoming": 0, "next_outgoing": 200 }));
        post(&reset, REASON, StatusCode::CONFLICT).await;
        assert_eq!(fixture.last_result(), "failed: the session is connected; disconnect it first");

        fixture.expect(&format!("/api/sessions/{FUNDA}/pause"), REASON, StatusCode::NO_CONTENT).await;
        fixture.wait_for_funda(false).await;
        assert_eq!(numbers(&post(&reset, REASON, StatusCode::OK).await), (Some(1), Some(1)));
        assert_eq!(fixture.last_result(), "allowed");
        assert_eq!(last_params(), serde_json::json!({ "next_incoming": 1, "next_outgoing": 1 }));
        let body = post(&sequence, r#"{"reason":"r","next_incoming":7}"#, StatusCode::OK).await;
        assert_eq!(numbers(&body), (Some(7), Some(1)));

        // The venue never logs on, as nothing listens on its address, so it resets.
        let venue = fixture.venue().await;
        let both = r#"{"reason":"r","next_incoming":5,"next_outgoing":9}"#;
        let body = post(&format!("/api/sessions/{venue}/sequence"), both, StatusCode::OK).await;
        assert_eq!(numbers(&body), (Some(5), Some(9)));
        let body = post(&format!("/api/sessions/{venue}/reset"), REASON, StatusCode::OK).await;
        assert_eq!(numbers(&body), (Some(1), Some(1)));
    }

    /// A cancel goes to the book, which starts the gateway's cancel; an order unknown is 404,
    /// one done 409, recorded as failed. An ID that isn't UTF-8 is refused, and recorded.
    #[tokio::test]
    async fn orders_cancel() {
        let fixture = fixture().await;
        fixture.expect("/api/orders/NOPE1/cancel", REASON, StatusCode::NOT_FOUND).await;
        let body = fixture.expect("/api/orders/%FF/cancel", REASON, StatusCode::BAD_REQUEST).await;
        assert!(body["error"].is_string(), "{body}");
        assert!(fixture.last_result().starts_with("refused: "), "{}", fixture.last_result());
        let body = fixture.expect("/api/sessions/%FF/pause", REASON, StatusCode::BAD_REQUEST).await;
        assert!(body["error"].is_string(), "{body}");
        assert!(fixture.last_result().starts_with("refused: "), "{}", fixture.last_result());
        let order = fixture.order_name().await;
        let cancel = format!("/api/orders/{order}/cancel");
        fixture.expect(&cancel, REASON, StatusCode::NO_CONTENT).await;
        let gateway = fixture.service.gateway();
        assert_eq!(gateway.cancelling().iter().map(|(id, _)| *id).collect::<Vec<_>>(), [OrderId(1)]);

        let detail = gateway.order(OrderId(1)).unwrap();
        let report = VenueReport {
            downstream: detail.requests[1].downstream.clone(),
            exec_type: ExecType::Canceled,
            status: Some(Status::Canceled),
            last_qty: Decimal::ZERO,
            last_px: Decimal::ZERO,
            cum_qty: Decimal::ZERO,
            avg_px: Decimal::ZERO,
            exec_id: "V1".into(),
            text: None,
            venue_order_id: None,
            ord_rej_reason: None,
        };
        crate::edge::tests::book_input(gateway, Input::VenueReport { venue: "NASDAQ".into(), report });
        assert_eq!(gateway.order(OrderId(1)).unwrap().order.status, Status::Canceled);
        fixture.expect(&cancel, REASON, StatusCode::CONFLICT).await;
        let results = fixture.results();
        assert_eq!(results[results.len() - 2..], ["allowed", "failed: the order is done"]);
    }

    /// A status request for an unknown order is 404; for a live one whose venue isn't logged on,
    /// 409, recorded as failed with why.
    #[tokio::test]
    async fn orders_status() {
        let fixture = fixture().await;
        fixture.expect("/api/orders/NOPE1/status", REASON, StatusCode::NOT_FOUND).await;
        assert_eq!(fixture.last_result(), "refused: no order NOPE1");
        let order = fixture.order_name().await;
        fixture.expect(&format!("/api/orders/{order}/status"), REASON, StatusCode::CONFLICT).await;
        assert_eq!(fixture.last_result(), format!("failed: {}", StatusError::VenueUnavailable));
    }

    /// Blocks and unblocks report whether they changed anything; the kill switch blocks and
    /// cancels. A scope that doesn't parse, or names no session, is refused, as is a body that
    /// isn't JSON.
    #[tokio::test]
    async fn risk_block_unblock_and_cancel_all() {
        let fixture = fixture().await;
        let gateway = fixture.service.gateway();
        let funda = r#"{"reason":"testing","scope":"comp:FUNDA"}"#;
        assert_eq!(fixture.expect("/api/risk/block", funda, StatusCode::OK).await["changed"], true);
        assert_eq!(gateway.blocked(), [Scope::CompId("FUNDA".into())]);
        assert_eq!(fixture.expect("/api/risk/block", funda, StatusCode::OK).await["changed"], false);
        assert_eq!(fixture.last_result(), "unchanged: already blocked");
        assert_eq!(fixture.expect("/api/risk/unblock", funda, StatusCode::OK).await["changed"], true);
        assert!(gateway.blocked().is_empty());
        assert_eq!(fixture.expect("/api/risk/unblock", funda, StatusCode::OK).await["changed"], false);
        assert_eq!(fixture.last_result(), "unchanged: not blocked");

        let session = r#"{"reason":"testing","scope":"session:FIX.4.4:JETBRIDGE->FUNDA"}"#;
        fixture.expect("/api/risk/cancel-all", session, StatusCode::NO_CONTENT).await;
        let blocked = gateway.blocked();
        let [Scope::Session(client)] = blocked.as_slice() else { panic!("{blocked:?}") };
        assert_eq!(client.comp_id(), "FUNDA");
        assert_eq!(gateway.cancelling().len(), 1);

        for (body, status) in [
            (r#"{"reason":"testing","scope":"FUNDA"}"#, StatusCode::BAD_REQUEST),
            (r#"{"reason":"testing"}"#, StatusCode::BAD_REQUEST),
            (r#"{"scope":"all"}"#, StatusCode::BAD_REQUEST),
            ("not json", StatusCode::BAD_REQUEST),
            (r#"{"reason":"testing","scope":"session:nobody"}"#, StatusCode::NOT_FOUND),
        ] {
            fixture.expect("/api/risk/block", body, status).await;
            assert!(fixture.last_result().starts_with("refused: "), "{body}");
        }
        assert_eq!(fixture.last_result(), "refused: no session:nobody");
    }

    /// An operator scoped to FUNDB acts on FUNDB, and is refused `all`, FUNDA's order and
    /// session, and the venue's session.
    #[tokio::test]
    async fn a_scoped_operator_acts_only_on_its_clients() {
        let fixture = fixture().await;
        let as_scoped = async |path: &str, body: &str| fixture.post(&fixture.scoped, path, body).await.status;
        let scope = |scope: &str| format!(r#"{{"reason":"testing","scope":"{scope}"}}"#);
        assert_eq!(as_scoped("/api/risk/block", &scope("comp:FUNDB")).await, StatusCode::OK);
        assert_eq!(as_scoped("/api/risk/block", &scope("all")).await, StatusCode::FORBIDDEN);
        assert_eq!(fixture.last_result(), "refused: sam may act only on clients FUNDB, not all");
        assert_eq!(as_scoped("/api/risk/cancel-all", &scope("comp:FUNDA")).await, StatusCode::FORBIDDEN);
        let order = fixture.order_name().await;
        assert_eq!(as_scoped(&format!("/api/orders/{order}/cancel"), REASON).await, StatusCode::FORBIDDEN);
        assert_eq!(as_scoped(&format!("/api/orders/{order}/status"), REASON).await, StatusCode::FORBIDDEN);
        assert_eq!(as_scoped(&format!("/api/sessions/{FUNDA}/pause"), REASON).await, StatusCode::FORBIDDEN);
        let venue = fixture.venue().await;
        assert_eq!(as_scoped(&format!("/api/sessions/{venue}/pause"), REASON).await, StatusCode::FORBIDDEN);
        assert_eq!(fixture.service.gateway().blocked(), [Scope::CompId("FUNDB".into())]);
        assert!(fixture.service.gateway().cancelling().is_empty());
        assert!(!fixture.service.client_paused().iter().any(|id| id.target_comp_id == "FUNDA"));
    }

    /// An admin scoped to FUNDB edits FUNDB's limits, and is refused FUNDA's, the defaults and a
    /// reload, which reach every client as `all` does; the file is left as it was.
    #[tokio::test]
    async fn a_scoped_admin_edits_only_its_clients_limits() {
        let fixture = fixture().await;
        let as_scoped = async |method: &str, path: &str, body: &str| {
            fixture.send(method, &fixture.scoped_admin, path, body).await.status
        };
        let credit = r#"{"reason":"testing","set":{"credit":5}}"#;
        let fundb = "/api/risk/limits/client/FUNDB";
        assert_eq!(as_scoped("PATCH", fundb, credit).await, StatusCode::OK);
        let config = fixture.config();
        assert_eq!(as_scoped("PATCH", CLIENT_FUNDA, credit).await, StatusCode::FORBIDDEN);
        assert_eq!(fixture.last_result(), "refused: abe may act only on clients FUNDB, not FUNDA");
        assert_eq!(as_scoped("DELETE", CLIENT_FUNDA, REASON).await, StatusCode::FORBIDDEN);
        assert_eq!(as_scoped("PATCH", DEFAULTS, credit).await, StatusCode::FORBIDDEN);
        assert_eq!(fixture.last_result(), "refused: abe may act only on clients FUNDB, not risk.defaults");
        assert_eq!(as_scoped("POST", "/api/reload", REASON).await, StatusCode::FORBIDDEN);
        assert_eq!(fixture.last_result(), "refused: abe may act only on clients FUNDB, not a reload");
        assert_eq!(fixture.config(), config);
        assert_eq!(as_scoped("DELETE", fundb, REASON).await, StatusCode::OK);
    }

    /// A reload answers what changed; one whose configuration is refused answers 422 with why,
    /// recorded as failed.
    #[tokio::test]
    async fn a_reload_answers_what_changed_or_why_not() {
        let fixture = fixture().await;
        let dir = fixture.dirs.0.path();
        let sessions = std::fs::read_to_string(dir.join("sessions.toml")).unwrap();
        std::fs::write(dir.join("sessions.toml"), format!("{sessions}[counterparty.FUNDB]\n")).unwrap();
        let answer = fixture.post(&fixture.admin, "/api/reload", REASON).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        assert_eq!(answer.body["sessions"]["added"], serde_json::json!(["FUNDB"]));
        assert_eq!(answer.body["config"]["routes"], 1);
        assert_eq!(answer.body["warnings"], serde_json::json!([]));

        std::fs::write(dir.join("sessions.toml"), format!("{sessions}[counterparty.FUNDC]\n")).unwrap();
        let config = "sessions = \"sessions.toml\"\n[[route]]\nvenue = \"NOWHERE\"\n";
        std::fs::write(dir.join("jetbridge.toml"), config).unwrap();
        let answer = fixture.post(&fixture.admin, "/api/reload", REASON).await;
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", answer.body);
        let error = answer.body["error"].as_str().unwrap();
        let took = "took the sessions file (added FUNDC; removed FUNDB); kept the routes, limits and day_end: ";
        assert!(error.starts_with(took), "{error}");
        assert_eq!(fixture.last_result(), format!("failed: {error}"));
    }

    const DEFAULTS: &str = "/api/risk/limits/defaults";
    const CLIENT_FUNDA: &str = "/api/risk/limits/client/FUNDA";

    /// Sets, changes and removes keys, in the defaults and a new client table, and deletes the
    /// table: each answers the table as the file has it and what the reload took, applies to the
    /// gateway's checks, and is audited with the keys set and removed. The comments and layout
    /// outside the table edited stay byte for byte.
    #[tokio::test]
    async fn limits_are_set_changed_removed_and_deleted() {
        let fixture = fixture().await;
        let top = "# Jetbridge\nsessions = \"sessions.toml\"  # beside this file\n\n[[route]]\nvenue = \"NASDAQ\"\n";
        std::fs::write(fixture.dirs.0.path().join("jetbridge.toml"), top).unwrap();
        let set = |set: &str| format!(r#"{{"reason":"testing","set":{set}}}"#);
        let funda = || fixture.service.gateway().limits().limits("FUNDA").clone();

        let body =
            fixture.admin("PATCH", DEFAULTS, &set(r#"{"credit":1000,"symbols":["AAPL"]}"#), StatusCode::OK).await;
        assert_eq!(body["table"], serde_json::json!({ "symbols": ["AAPL"], "credit": 1000 }));
        assert_eq!(body["reload"]["config"]["routes"], 1);
        assert_eq!(fixture.config(), format!("{top}\n[risk.defaults]\ncredit = 1000\nsymbols = [\"AAPL\"]\n"));
        assert_eq!(funda().credit, Some(Decimal::from(1000)));
        let audit = std::fs::read_to_string(fixture.dirs.1.path().join("audit.jsonl")).unwrap();
        let line: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert_eq!((&line["action"], &line["target"]), (&"set_limits".into(), &"risk.defaults".into()));
        assert_eq!(
            line["params"],
            serde_json::json!({ "set": { "credit": 1000, "symbols": ["AAPL"] }, "removed": [] })
        );

        let body = fixture.admin("PATCH", CLIENT_FUNDA, &set(r#"{"credit":"250.5"}"#), StatusCode::OK).await;
        assert_eq!(body["table"], serde_json::json!({ "credit": "250.5" }));
        assert_eq!(funda().credit, Some("250.5".parse().unwrap()));
        assert_eq!(funda().symbols, Some(vec!["AAPL".into()]));
        fixture.admin("PATCH", CLIENT_FUNDA, &set(r#"{"credit":300,"self_trade":true}"#), StatusCode::OK).await;
        let body = fixture.admin("PATCH", CLIENT_FUNDA, &set(r#"{"credit":null}"#), StatusCode::OK).await;
        assert_eq!(body["table"], serde_json::json!({ "self_trade": true }));
        assert_eq!(funda().credit, Some(Decimal::from(1000)));
        let line = fixture.results().len();
        assert_eq!(fixture.ok("/api/risk/limits").await["clients"]["FUNDA"], serde_json::json!({ "self_trade": true }));
        assert_eq!(fixture.results().len(), line);

        let body = fixture.admin("DELETE", CLIENT_FUNDA, REASON, StatusCode::OK).await;
        assert_eq!(body["table"], serde_json::Value::Null);
        assert!(!funda().self_trade);
        assert_eq!(fixture.config(), format!("{top}\n[risk.defaults]\ncredit = 1000\nsymbols = [\"AAPL\"]\n"));
        fixture.admin("DELETE", CLIENT_FUNDA, REASON, StatusCode::NOT_FOUND).await;
        assert_eq!(fixture.last_result(), "failed: no [risk.client.FUNDA]");
        let body = fixture.admin("PATCH", DEFAULTS, &set(r#"{"credit":null,"symbols":null}"#), StatusCode::OK).await;
        assert_eq!(body["table"], serde_json::json!({}));
        assert_eq!(*fixture.service.gateway().limits().limits("FUNDA"), crate::risk::Limits::default());
    }

    /// A key or a type a risk table doesn't take, nothing to set, or a CompID that isn't plain,
    /// is 400; a value the loader refuses (a negative credit) is 422, the file left as it was.
    /// Each refusal is audited.
    #[tokio::test]
    async fn a_bad_limit_edit_is_refused_and_changes_nothing() {
        let fixture = fixture().await;
        let before = fixture.config();
        for (path, body, status) in [
            (DEFAULTS, r#"{"reason":"r","set":{"max_qty":5}}"#, StatusCode::BAD_REQUEST),
            (DEFAULTS, r#"{"reason":"r","set":{"credit":true}}"#, StatusCode::BAD_REQUEST),
            (DEFAULTS, r#"{"reason":"r"}"#, StatusCode::BAD_REQUEST),
            (DEFAULTS, r#"{"set":{"credit":5}}"#, StatusCode::BAD_REQUEST),
            ("/api/risk/limits/client/FUND.A", r#"{"reason":"r","set":{"credit":5}}"#, StatusCode::BAD_REQUEST),
            (DEFAULTS, r#"{"reason":"r","set":{"credit":-5}}"#, StatusCode::UNPROCESSABLE_ENTITY),
            (CLIENT_FUNDA, r#"{"reason":"r","set":{"symbols":[]}}"#, StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            fixture.admin("PATCH", path, body, status).await;
            assert_eq!(fixture.config(), before, "{body}");
        }
        let results = fixture.results();
        assert_eq!(results[0], "refused: unknown key \"max_qty\"");
        assert!(results.last().unwrap().contains("risk.client.FUNDA.symbols: empty"), "{results:?}");
        assert!(results[results.len() - 3].ends_with("risk.defaults.credit: -5 is not positive"), "{results:?}");
    }

    /// A file a hand edit broke, as TOML or as a configuration, refuses every edit until it's
    /// fixed, and is left as it was.
    #[tokio::test]
    async fn a_file_broken_by_hand_refuses_every_edit() {
        let fixture = fixture().await;
        let path = fixture.dirs.0.path().join("jetbridge.toml");
        let good = format!("{}[risk.client.FUNDA]\ncredit = 1\n", fixture.config());
        let credit = r#"{"reason":"r","set":{"credit":5}}"#;
        for broken in [format!("{good}[[route]\n"), format!("{good}[[route]]\nvenue = \"NOWHERE\"\n")] {
            std::fs::write(&path, &broken).unwrap();
            let body = fixture.admin("PATCH", DEFAULTS, credit, StatusCode::UNPROCESSABLE_ENTITY).await;
            assert!(body["error"].as_str().unwrap().contains("jetbridge.toml: "), "{body}");
            fixture.admin("DELETE", CLIENT_FUNDA, REASON, StatusCode::UNPROCESSABLE_ENTITY).await;
            assert_eq!(fixture.config(), broken);
        }
        std::fs::write(&path, &good).unwrap();
        fixture.admin("PATCH", DEFAULTS, credit, StatusCode::OK).await;
    }

    /// An edit whose reload keeps the broken sessions file is written all the same, and answered
    /// and audited as failed saying so.
    #[tokio::test]
    async fn an_edit_whose_reload_keeps_a_file_is_written() {
        let fixture = fixture().await;
        std::fs::write(fixture.dirs.0.path().join("sessions.toml"), "[acceptor\n").unwrap();
        let body = fixture
            .admin("PATCH", DEFAULTS, r#"{"reason":"r","set":{"credit":5}}"#, StatusCode::UNPROCESSABLE_ENTITY)
            .await;
        let written = "the edit is written, but the reload kept the sessions file: ";
        assert!(body["error"].as_str().unwrap().starts_with(written), "{body}");
        assert!(fixture.last_result().starts_with(&format!("failed: {written}")), "{}", fixture.last_result());
        assert!(fixture.config().contains("credit = 5"));
    }
}
