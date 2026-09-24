//! The processed rule list a key is judged by, with the live cooldowns each entry covers, and the cooldown
//! state grouped by model with the ledger's last rejections and permits.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{recent_events_of_kind, EventRecord};
use phoebis_routing::{pattern, rules};
use phoebis_store::CooldownRow;
use serde_json::{json, Value};

use crate::analytics::ledger_failure;
use crate::routes::{failure, store_failure};
use crate::AdminState;

/// The newest rejections and permits a state read answers, each event kind on its own.
const RECENT_EVENTS: usize = 50;

const ROUTER_UNAVAILABLE: &str = "the router is not available";

fn cooldown_json(row: &CooldownRow) -> Value {
    json!({
        "account_id": row.account_id,
        "scope": row.scope,
        "reason": row.reason,
        "until": row.until,
        "window": row.window,
        "set_at": row.set_at,
    })
}

/// The model a `model:`-scoped row cools, or `None` for an account-wide row.
fn model_of(scope: &str) -> Option<&str> {
    scope.strip_prefix("model:")
}

pub(crate) async fn key_models(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    let key = match state.store.api_key(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "key not found"),
        Err(e) => return store_failure(e),
    };
    // A key's pool is a foreign key the store enforces (a pool with keys refuses deletion), so a missing pool
    // here would be the same broken reference a missing key already answers.
    let pool = match state.store.pool(&key.pool_id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "key not found"),
        Err(e) => return store_failure(e),
    };
    let members = match state.store.pool_members(&pool.id) {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    let cooldowns = match state.router.cooldowns().await {
        Ok(rows) => rows,
        Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, ROUTER_UNAVAILABLE),
    };
    let now = (state.clock)();
    let processed = rules::process(&key.model_rules, key.model_rules_mode, &pool.model_rules);
    let rule_rows: Vec<Value> = processed
        .iter()
        .map(|entry| {
            let mut matching: Vec<&CooldownRow> = cooldowns
                .iter()
                .filter(|c| {
                    c.until > now
                        && members.iter().any(|m| m.account_id == c.account_id)
                        && model_of(&c.scope)
                            .is_some_and(|model| pattern::matches(&entry.rule.pattern, model))
                })
                .collect();
            matching.sort_by(|a, b| a.account_id.cmp(&b.account_id));
            json!({
                "level": entry.level.as_str(),
                "pattern": entry.rule.pattern,
                "not": entry.rule.not,
                "cooldowns": matching.into_iter().map(cooldown_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({
        "key_id": key.id,
        "pool_id": key.pool_id,
        "model_rules_mode": key.model_rules_mode.as_str(),
        "long_context_twins": key.long_context_twins,
        "rules": rule_rows,
    }))
    .into_response()
}

pub(crate) async fn state(State(state): State<Arc<AdminState>>) -> Response {
    let cooldowns = match state.router.cooldowns().await {
        Ok(rows) => rows,
        Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, ROUTER_UNAVAILABLE),
    };
    let now = (state.clock)();
    let mut grouped: BTreeMap<&str, Vec<&CooldownRow>> = BTreeMap::new();
    for row in &cooldowns {
        if row.until <= now {
            continue;
        }
        if let Some(model) = model_of(&row.scope) {
            grouped.entry(model).or_default().push(row);
        }
    }
    let cooldown_groups: Vec<Value> = grouped
        .into_iter()
        .map(|(model, mut rows)| {
            rows.sort_by(|a, b| a.account_id.cmp(&b.account_id));
            json!({
                "model": model,
                "rows": rows.into_iter().map(cooldown_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    let events = state
        .ledger
        .db()
        .read(move |c| {
            Ok((
                recent_events_of_kind(c, "model_rejected", RECENT_EVENTS)?,
                recent_events_of_kind(c, "model_permitted", RECENT_EVENTS)?,
            ))
        })
        .await;
    let (rejections, permits) = match events {
        Ok(pair) => pair,
        Err(e) => return ledger_failure(e),
    };
    Json(json!({
        "cooldowns": cooldown_groups,
        "rejections": rejections.iter().map(rejection_json).collect::<Vec<_>>(),
        "permits": permits.iter().map(permit_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn detail_of(event: &EventRecord) -> Value {
    event
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or_else(|| json!({}))
}

fn rejection_json(event: &EventRecord) -> Value {
    let detail = detail_of(event);
    json!({
        "at": event.at,
        "account_id": event.account_id,
        "key_id": event.key_id,
        "session_id": event.session_id,
        "model": event.model,
        "wire_model": detail.get("wire_model").cloned().unwrap_or(Value::Null),
        "request_id": detail.get("source_request_id").cloned().unwrap_or(Value::Null),
        "upstream_request_id": detail.get("request_id").cloned().unwrap_or(Value::Null),
    })
}

fn permit_json(event: &EventRecord) -> Value {
    let detail = detail_of(event);
    json!({
        "at": event.at,
        "account_id": event.account_id,
        "key_id": event.key_id,
        "session_id": event.session_id,
        "model": event.model,
        "wire_model": detail.get("wire_model").cloned().unwrap_or(Value::Null),
        "request_id": detail.get("source_request_id").cloned().unwrap_or(Value::Null),
        "upstream_request_id": detail.get("request_id").cloned().unwrap_or(Value::Null),
        "rejected_by": detail.get("rejected_by").cloned().unwrap_or_else(|| json!([])),
    })
}
