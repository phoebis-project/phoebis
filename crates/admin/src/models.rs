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
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{ErrorBody, LedgerFailure, RouterUnavailable, StorageFailure};
use crate::routes::{failure, store_failure};
use crate::AdminState;

/// The newest rejections and permits a state read answers, each event kind on its own.
const RECENT_EVENTS: usize = 50;

const ROUTER_UNAVAILABLE: &str = "the router is not available";

/// The processed rule list a key is judged by, in the order it applies.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyModels {
    pub key_id: String,
    pub pool_id: String,
    /// `extend` or `replace`.
    #[schema(example = "extend")]
    pub model_rules_mode: String,
    pub long_context_twins: bool,
    pub rules: Vec<RuleEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleEntry {
    /// `key` or `pool`.
    #[schema(example = "key")]
    pub level: String,
    pub pattern: String,
    pub not: bool,
    /// For an allow entry, the live model-scoped cooldowns of the pool's members whose model the pattern
    /// matches. Empty for a deny entry, since a denied model is never routed.
    pub cooldowns: Vec<ModelCooldown>,
}

/// A live model-scoped cooldown.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelCooldown {
    pub account_id: String,
    pub scope: String,
    pub reason: String,
    pub until: i64,
    pub window: String,
    pub set_at: i64,
}

/// The cooldown state grouped by model with the ledger's last rejections and permits.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelsState {
    /// One group per canonical model with a live model-scoped cooldown on any account. Account-wide cooldowns are
    /// on the account rows instead.
    pub cooldowns: Vec<ModelCooldowns>,
    /// The newest 50 `model_rejected` events, newest first.
    pub rejections: Vec<Rejection>,
    /// The newest 50 `model_permitted` events, newest first.
    pub permits: Vec<Permit>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelCooldowns {
    pub model: String,
    pub rows: Vec<ModelCooldown>,
}

/// A `model_rejected` event.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rejection {
    pub at: i64,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    #[schema(value_type = Option<String>)]
    pub wire_model: Value,
    /// The router's own id for the request.
    #[schema(value_type = Option<String>)]
    pub request_id: Value,
    /// The upstream's own id for the rejected attempt.
    #[schema(value_type = Option<String>)]
    pub upstream_request_id: Value,
}

/// A `model_permitted` event.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Permit {
    pub at: i64,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    #[schema(value_type = Option<String>)]
    pub wire_model: Value,
    /// The router's own id for the request.
    #[schema(value_type = Option<String>)]
    pub request_id: Value,
    /// The upstream's own id for the permitted attempt.
    #[schema(value_type = Option<String>)]
    pub upstream_request_id: Value,
    #[schema(value_type = Vec<String>)]
    pub rejected_by: Value,
}

fn cooldown_json(row: &CooldownRow) -> ModelCooldown {
    ModelCooldown {
        account_id: row.account_id.clone(),
        scope: row.scope.clone(),
        reason: row.reason.clone(),
        until: row.until,
        window: row.window.clone(),
        set_at: row.set_at,
    }
}

/// The model a `model:`-scoped row cools, or `None` for an account-wide row.
fn model_of(scope: &str) -> Option<&str> {
    scope.strip_prefix("model:")
}

/// Read the key's processed model rules.
#[utoipa::path(
    get,
    path = "/admin/keys/{id}/models",
    operation_id = "key_models",
    tag = "Keys",
    params(("id" = String, Path, description = "The key's id")),
    responses(
        (status = 200, description = "The key's processed rules.", body = KeyModels),
        (status = 404, description = "`key not found`", body = ErrorBody, example = json!({"error": "key not found"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RouterUnavailable),
    ),
)]
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
    let rule_rows: Vec<RuleEntry> = processed
        .iter()
        .map(|entry| {
            // A denied model is never routed, so its cooldowns say nothing about the key.
            let mut matching: Vec<&CooldownRow> = if entry.rule.not {
                Vec::new()
            } else {
                cooldowns
                    .iter()
                    .filter(|c| {
                        c.until > now
                            && members.iter().any(|m| m.account_id == c.account_id)
                            && model_of(&c.scope)
                                .is_some_and(|model| pattern::matches(&entry.rule.pattern, model))
                    })
                    .collect()
            };
            matching.sort_by(|a, b| a.account_id.cmp(&b.account_id));
            RuleEntry {
                level: entry.level.as_str().to_string(),
                pattern: entry.rule.pattern.clone(),
                not: entry.rule.not,
                cooldowns: matching.into_iter().map(cooldown_json).collect(),
            }
        })
        .collect();
    Json(KeyModels {
        key_id: key.id,
        pool_id: key.pool_id,
        model_rules_mode: key.model_rules_mode.as_str().to_string(),
        long_context_twins: key.long_context_twins,
        rules: rule_rows,
    })
    .into_response()
}

/// Read the model cooldowns and the last rejections and permits.
#[utoipa::path(
    get,
    path = "/admin/models/state",
    operation_id = "models_state",
    tag = "Models",
    responses(
        (status = 200, description = "The model state.", body = ModelsState),
        (status = 500, response = LedgerFailure),
        (status = 503, response = RouterUnavailable),
    ),
)]
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
    let cooldown_groups: Vec<ModelCooldowns> = grouped
        .into_iter()
        .map(|(model, mut rows)| {
            rows.sort_by(|a, b| a.account_id.cmp(&b.account_id));
            ModelCooldowns {
                model: model.to_string(),
                rows: rows.into_iter().map(cooldown_json).collect(),
            }
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
    Json(ModelsState {
        cooldowns: cooldown_groups,
        rejections: rejections.iter().map(rejection_json).collect(),
        permits: permits.iter().map(permit_json).collect(),
    })
    .into_response()
}

fn detail_of(event: &EventRecord) -> Value {
    event
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or_else(|| json!({}))
}

fn rejection_json(event: &EventRecord) -> Rejection {
    let detail = detail_of(event);
    Rejection {
        at: event.at,
        account_id: event.account_id.clone(),
        key_id: event.key_id.clone(),
        session_id: event.session_id.clone(),
        model: event.model.clone(),
        wire_model: detail.get("wire_model").cloned().unwrap_or(Value::Null),
        request_id: detail
            .get("source_request_id")
            .cloned()
            .unwrap_or(Value::Null),
        upstream_request_id: detail.get("request_id").cloned().unwrap_or(Value::Null),
    }
}

fn permit_json(event: &EventRecord) -> Permit {
    let detail = detail_of(event);
    Permit {
        at: event.at,
        account_id: event.account_id.clone(),
        key_id: event.key_id.clone(),
        session_id: event.session_id.clone(),
        model: event.model.clone(),
        wire_model: detail.get("wire_model").cloned().unwrap_or(Value::Null),
        request_id: detail
            .get("source_request_id")
            .cloned()
            .unwrap_or(Value::Null),
        upstream_request_id: detail.get("request_id").cloned().unwrap_or(Value::Null),
        rejected_by: detail
            .get("rejected_by")
            .cloned()
            .unwrap_or_else(|| json!([])),
    }
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::round_trips;

    #[test]
    fn the_key_models_answer_round_trips_through_its_declared_type() {
        round_trips::<super::KeyModels>("key_models");
    }

    #[test]
    fn the_models_state_answer_round_trips_through_its_declared_type() {
        round_trips::<super::ModelsState>("models_state");
    }
}
