//! The routing log: the merged page of decisions and events under the view's filters, and one decision's record.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    decision_record, list_routing_log, DecisionListRow, EventListRow, RoutingLogCursor,
    RoutingLogFilter, RoutingLogRow, DECISION_KINDS, EVENT_KINDS, PROBE_KIND,
};
use serde_json::{json, Map, Value};

use crate::analytics::ledger_failure;
use crate::requests::{decode_diff, list_param};
use crate::routes::failure;
use crate::AdminState;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 200;
const HOUR_MS: i64 = 3_600_000;

/// The view's URL keys, alphabetical. A key outside them is refused rather than ignored, so a link built for another
/// view never reads as a filter that took.
const KEYS: &[&str] = &[
    "accounts", "cursor", "from", "keys", "kinds", "limit", "models", "pools", "request",
    "session", "to",
];

/// A value that parses as an integer is Unix milliseconds, else an RFC 3339 instant.
fn time(q: &HashMap<String, String>, name: &'static str) -> Result<Option<i64>, String> {
    let Some(raw) = q.get(name) else {
        return Ok(None);
    };
    if let Ok(ms) = raw.parse::<i64>() {
        return Ok(Some(ms));
    }
    jiff::Timestamp::from_str(raw)
        .map(|t| Some(t.as_millisecond()))
        .map_err(|_| format!("{name} is not a time"))
}

pub(crate) async fn list(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut names: Vec<&String> = q.keys().collect();
    names.sort();
    if let Some(unknown) = names
        .into_iter()
        .find(|name| !KEYS.contains(&name.as_str()))
    {
        return failure(
            StatusCode::BAD_REQUEST,
            &format!("unknown filter {unknown}"),
        );
    }
    let now = (state.clock)();
    let (from, to) = match (time(&q, "from"), time(&q, "to")) {
        (Ok(from), Ok(to)) => (from.unwrap_or(now - HOUR_MS), to.unwrap_or(now)),
        (Err(error), _) | (_, Err(error)) => return failure(StatusCode::BAD_REQUEST, &error),
    };
    let kinds = q.contains_key("kinds").then(|| list_param(&q, "kinds"));
    if let Some(unknown) = kinds.iter().flatten().find(|kind| {
        let kind = kind.as_str();
        kind != PROBE_KIND && !DECISION_KINDS.contains(&kind) && !EVENT_KINDS.contains(&kind)
    }) {
        return failure(StatusCode::BAD_REQUEST, &format!("unknown kind {unknown}"));
    }
    let limit = match q.get("limit").map(|raw| raw.parse::<usize>()) {
        None => DEFAULT_LIMIT,
        Some(Ok(n)) if (1..=MAX_LIMIT).contains(&n) => n,
        Some(_) => return failure(StatusCode::BAD_REQUEST, "limit is 1 to 200"),
    };
    let cursor = match q.get("cursor").map(|raw| RoutingLogCursor::parse(raw)) {
        None => None,
        Some(Some(cursor)) => Some(cursor),
        Some(None) => return failure(StatusCode::BAD_REQUEST, "cursor is <at>:<rank>:<id>"),
    };
    let filter = RoutingLogFilter {
        from,
        to,
        // An emptied field in the view's bar narrows nothing, as an absent one.
        request: q.get("request").filter(|r| !r.is_empty()).cloned(),
        session: q.get("session").filter(|s| !s.is_empty()).cloned(),
        keys: list_param(&q, "keys"),
        pools: list_param(&q, "pools"),
        accounts: list_param(&q, "accounts"),
        models: list_param(&q, "models"),
        kinds,
    };
    let page = state
        .ledger
        .db()
        .read(move |c| list_routing_log(c, &filter, cursor.as_ref(), limit))
        .await;
    let page = match page {
        Ok(page) => page,
        Err(e) => return ledger_failure(e),
    };
    let mut answer = json!({
        "rows": page.rows.iter().map(row_json).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor,
    });
    if let Some((decisions, events)) = page.counts {
        answer["counts"] = json!({ "decisions": decisions, "events": events });
    }
    Json(answer).into_response()
}

fn row_json(row: &RoutingLogRow) -> Value {
    match row {
        RoutingLogRow::Decision(d) => decision_json(d),
        RoutingLogRow::Event(e) => event_json(e),
    }
}

/// The summary repeats the `kind` and `probe` columns; the columns' values stand, so no field is answered twice.
fn decision_json(d: &DecisionListRow) -> Value {
    let mut row = Map::new();
    row.insert("type".into(), "decision".into());
    let columns = json!({
        "request_id": d.request_id, "seq": d.seq, "attempt": d.attempt, "attempt_count": d.attempt_count,
        "at": d.at, "kind": d.kind, "probe": d.probe, "key_id": d.key_id, "pool_id": d.pool_id,
        "session_id": d.session_id, "agent_id": d.agent_id, "model": d.model, "account_id": d.account_id,
        "from_account_id": d.from_account_id,
    });
    if let Value::Object(columns) = columns {
        row.extend(columns);
    }
    if let Value::Object(summary) = &d.summary {
        for (name, value) in summary {
            row.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    row.insert("cache".into(), json!(d.cache));
    row.insert("status".into(), json!(d.status));
    Value::Object(row)
}

fn event_json(e: &EventListRow) -> Value {
    let mut detail = e.detail.clone();
    decode_diff(&mut detail);
    json!({
        "type": "event", "id": e.id, "at": e.at, "kind": e.kind, "account_id": e.account_id, "key_id": e.key_id,
        "pool_id": e.pool_id, "session_id": e.session_id, "agent_id": e.agent_id, "model": e.model,
        "request_id": e.request_id, "actor": e.actor, "detail": detail,
    })
}

/// Purged and never written answer the same 404: the row is the only witness either way.
pub(crate) async fn record(
    State(state): State<Arc<AdminState>>,
    Path((id, seq)): Path<(String, String)>,
) -> Response {
    let missing = || {
        failure(
            StatusCode::NOT_FOUND,
            &format!("no decision {seq} for request {id}"),
        )
    };
    let Ok(n) = seq.parse::<u32>() else {
        return missing();
    };
    let request_id = id.clone();
    match state
        .ledger
        .db()
        .read(move |c| decision_record(c, &request_id, n))
        .await
    {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => missing(),
        Err(e) => ledger_failure(e),
    }
}
