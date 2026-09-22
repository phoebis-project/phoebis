//! The request log: one row per client request with its attempt trail, the detail with its events, and the
//! captured bodies for administrators.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use phoebis_ledger::{
    list_requests, request_by_id, request_events, AttemptRecord, LiveKeys, RequestFilter,
    RequestRecord,
};
use serde_json::{json, Value};

use crate::analytics::ledger_failure;
use crate::routes::{failure, Caller};
use crate::{AdminState, Role};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

pub(crate) fn request_summary(r: &RequestRecord, attempts: &[AttemptRecord]) -> Value {
    let last = attempts.last();
    let tokens = |f: fn(&AttemptRecord) -> Option<u64>| last.and_then(f).unwrap_or(0);
    json!({
        "id": r.id, "at": r.received_at, "finished_at": r.finished_at, "key_id": r.key_id, "pool_id": r.pool_id,
        "account_id": last.map(|a| a.account_id.clone()), "session_id": r.session_id, "agent_id": r.agent_id, "parent_agent_id": r.parent_agent_id,
        "model": r.model, "canonical_model": r.canonical_model, "kind": r.kind, "origin": r.origin, "transport": if r.stream { "sse" } else { "json" },
        "status": r.status, "outcome": r.outcome, "error_code": r.error_code, "client_drift": r.client_drift, "ttfb_ms": r.ttfb_ms, "ttft_ms": r.ttfc_ms, "duration_ms": r.duration_ms,
        "request_bytes": r.request_bytes, "response_bytes": r.response_bytes, "kept_warm": r.kept_warm, "captured_bodies": r.bodies_captured,
        "request_truncated": r.request_truncated, "response_truncated": r.response_truncated,
        "user_agent": r.user_agent,
        "shape": {
            "context_edits": r.context_edits.split(',').filter(|e| !e.is_empty()).collect::<Vec<_>>(),
            "effort": r.effort, "max_tokens": r.max_tokens, "message_count": r.message_count,
            "requested_service_tier": r.requested_service_tier, "requested_speed": r.requested_speed,
            "system_blocks": r.system_blocks, "temperature": r.temperature, "thinking_budget": r.thinking_budget,
            "thinking_type": r.thinking_type, "tool_count": r.tool_count,
        },
        "usage": { "cache_read": tokens(|a| a.cache_read_tokens), "cache_write_1h": tokens(|a| a.cache_write_1h_tokens), "cache_write_5m": tokens(|a| a.cache_write_5m_tokens), "input": tokens(|a| a.input_tokens), "output": tokens(|a| a.output_tokens), "thinking": tokens(|a| a.thinking_tokens), "web_fetch": tokens(|a| a.web_fetch_requests), "web_search": tokens(|a| a.web_search_requests) },
        // Folded onto a positive zero rather than summed: `Sum<f64>` starts at -0.0, so a request with no
        // priced attempt would put a signed zero on the wire and the UI would read "-$0.00".
        "cost_usd": attempts.iter().filter_map(|a| a.estimated_usd).fold(0.0, |a, b| a + b),
        "attempts": attempts.iter().map(attempt_json).collect::<Vec<_>>(),
    })
}

/// One attempt of the trail. Its own function because the two objects together overflow the `json!` macro's
/// expansion depth.
fn attempt_json(a: &AttemptRecord) -> Value {
    let window = |id: &str, u: Option<f64>, reset: Option<i64>| {
        (u.is_some() || reset.is_some())
            .then(|| json!({ "id": id, "utilization": u, "resets_at": reset }))
    };
    let windows: Vec<Value> = [
        window("5h", a.ratelimit_5h_utilization, a.ratelimit_5h_reset),
        window("7d", a.ratelimit_7d_utilization, a.ratelimit_7d_reset),
        window(
            "7d_oi",
            a.ratelimit_7d_oi_utilization,
            a.ratelimit_7d_oi_reset,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    json!({
        "index": a.attempt, "account_id": a.account_id, "account_kind": a.account_kind, "outcome": a.outcome,
        "upstream_status": a.upstream_status, "upstream_request_id": a.upstream_request_id, "started_at": a.started_at,
        "duration_ms": a.finished_at.map(|f| f - a.started_at), "ttfb_ms": a.ttfb_ms, "ttft_ms": a.ttfc_ms,
        "cache_miss_reason": a.cache_miss_reason, "service_tier": a.service_tier, "client_drift": a.client_drift,
        "cooldown_set": a.cooldown_scope.as_ref().map(|scope| json!({ "reason": scope, "until": a.cooldown_until })),
        "windows": windows,
        "overage_in_use": a.overage_in_use, "estimated_usd": a.estimated_usd, "pricing_version": a.pricing_version,
        "response": {
            "cleared_input_tokens": a.cleared_input_tokens, "inference_geo": a.inference_geo,
            "refusal_category": a.refusal_category, "refusal_recommended_model": a.refusal_recommended_model,
            "served_model": a.served_model, "speed": a.speed, "stop_reason": a.stop_reason, "stop_sequence": a.stop_sequence,
        },
    })
}

fn list_param(q: &HashMap<String, String>, name: &str) -> Vec<String> {
    q.get(name)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn list(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let number = |name: &'static str| match q.get(name) {
        Some(v) => v.parse::<i64>().map(Some).map_err(|_| name),
        None => Ok(None),
    };
    let (from, to) = match (number("from"), number("to")) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(name), _) | (_, Err(name)) => {
            return failure(StatusCode::BAD_REQUEST, &format!("{name} is not a number"))
        }
    };
    let statuses: Result<Vec<u16>, _> = list_param(&q, "statuses")
        .iter()
        .map(|s| s.parse::<u16>())
        .collect();
    let Ok(statuses) = statuses else {
        return failure(StatusCode::BAD_REQUEST, "statuses are numbers");
    };
    let filter = RequestFilter {
        text: q.get("text").cloned(),
        keys: list_param(&q, "keys"),
        pools: list_param(&q, "pools"),
        accounts: list_param(&q, "accounts"),
        models: list_param(&q, "models"),
        kinds: list_param(&q, "kinds"),
        statuses,
        outcomes: list_param(&q, "outcomes"),
        user_agent: q.get("user_agent").cloned(),
        from,
        to,
        include_probes: q
            .get("include_probes")
            .is_some_and(|v| v == "true" || v == "1"),
    };
    if !phoebis_ledger::patterns_within_bounds(filter.user_agent.as_slice()) {
        return failure(StatusCode::BAD_REQUEST, phoebis_ledger::PATTERNS_TOO_LONG);
    }
    let limit = q
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let cursor = q.get("cursor").cloned();
    if cursor.as_deref().is_some_and(|c| {
        c.split_once(':')
            .is_none_or(|(at, _)| at.parse::<i64>().is_err())
    }) {
        return failure(StatusCode::BAD_REQUEST, "cursor is <received_at>:<id>");
    }
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    match state
        .ledger
        .db()
        .read(move |c| list_requests(c, &filter, &live, cursor.as_deref(), limit))
        .await
    {
        Ok((rows, next)) => Json(json!({ "requests": rows.iter().map(|(r, a)| request_summary(r, a)).collect::<Vec<_>>(), "next_cursor": next })).into_response(),
        Err(e) => ledger_failure(e),
    }
}

pub(crate) async fn show(State(state): State<Arc<AdminState>>, Path(id): Path<String>) -> Response {
    let found = state
        .ledger
        .db()
        .read(move |c| {
            let Some((r, a)) = request_by_id(c, &id)? else {
                return Ok(None);
            };
            let events = request_events(c, &r)?;
            Ok(Some((r, a, events)))
        })
        .await;
    match found {
        Ok(Some((r, a, events))) => Json(json!({ "request": request_summary(&r, &a), "events": events.iter().map(event_json).collect::<Vec<_>>() })).into_response(),
        Ok(None) => failure(StatusCode::NOT_FOUND, "no such request"),
        Err(e) => ledger_failure(e),
    }
}

pub(crate) fn event_json(e: &phoebis_ledger::EventRecord) -> Value {
    let mut detail = e
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
        .unwrap_or(Value::Null);
    // A row the mirror wrote before `registry_changed`'s diff became a value still carries it as a JSON
    // string; decoded here so every row reads the same.
    if let Some(diff) = detail
        .get("diff")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    {
        detail["diff"] = diff;
    }
    json!({ "at": e.at, "kind": e.kind, "account_id": e.account_id, "key_id": e.key_id, "session_id": e.session_id, "model": e.model, "actor": e.actor, "detail": detail })
}

pub(crate) async fn bodies(
    State(state): State<Arc<AdminState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Response {
    // The gate lets a viewer through on every GET; bodies are prompts, so this one route checks the role itself.
    if caller.role != Role::Admin {
        return failure(
            StatusCode::FORBIDDEN,
            "captured bodies are for administrators",
        );
    }
    let as_json_or_text = |bytes: &[u8]| {
        serde_json::from_slice::<Value>(bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
    };
    match state.ledger.db().bodies(&id).await {
        // The byte counts are read off the raw vectors before rendering: a re-encoded JSON string adds quotes
        // and escapes, and a JavaScript `.length` on it counts UTF-16 units, so neither measures what was cut.
        Ok(Some(b)) => Json(json!({
            "request_id": b.request_id, "request": as_json_or_text(&b.request_body), "response": b.response_body.as_deref().map(as_json_or_text),
            "request_bytes_stored": b.request_body.len(), "request_bytes_seen": b.request_len_seen,
            "response_bytes_stored": b.response_body.as_ref().map_or(0, Vec::len), "response_bytes_seen": b.response_len_seen,
            "request_truncated": (b.request_body.len() as u64) < b.request_len_seen,
            "response_truncated": b.response_body.as_ref().is_some_and(|body| (body.len() as u64) < b.response_len_seen),
            "expires_at": b.expires_at,
        })).into_response(),
        Ok(None) => failure(StatusCode::NOT_FOUND, "no captured bodies for this request"),
        Err(e) => ledger_failure(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_ledger::RequestRecord;

    #[test]
    fn a_request_with_no_priced_attempt_costs_an_unsigned_zero() {
        let summary = request_summary(&RequestRecord::default(), &[]);
        assert_eq!(
            summary["cost_usd"].to_string(),
            "0.0",
            "a signed zero on the wire is read back as -$0.00"
        );
    }
}
