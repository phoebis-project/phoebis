//! The processed rule list a key is judged by, with the live cooldowns each entry covers.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_routing::{pattern, rules};
use phoebis_store::CooldownRow;
use utoipa::ToSchema;

use crate::openapi::{ErrorBody, RouterUnavailable, StorageFailure};
use crate::routes::{api_key, found, router_unavailable, store_failure};
use crate::AdminState;

/// The processed rule list a key is judged by, in the order it applies.
#[derive(serde::Serialize, ToSchema)]
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

#[derive(serde::Serialize, ToSchema)]
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
#[derive(serde::Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelCooldown {
    pub account_id: String,
    pub scope: String,
    pub reason: String,
    pub until: i64,
    pub window: String,
    pub set_at: i64,
}

fn cooldown_json(row: &CooldownRow) -> ModelCooldown {
    ModelCooldown {
        account_id: row.account_id.clone(),
        scope: row.scope.to_string(),
        reason: row.reason.clone(),
        until: row.until,
        window: row.window.clone(),
        set_at: row.set_at,
    }
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
    let key = match api_key(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    // A key's pool is a foreign key the store enforces (a pool with keys refuses deletion), so a missing pool
    // here would be the same broken reference a missing key already answers.
    let pool = match found(state.store.pool(&key.pool_id), "key not found") {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let members = match state.store.pool_members(&pool.id) {
        Ok(rows) => rows,
        Err(e) => return store_failure(e),
    };
    let cooldowns = match state.router.cooldowns().await {
        Ok(rows) => rows,
        Err(_) => return router_unavailable(),
    };
    let now = (state.clock)();
    // Sorted once and stably, so each entry's filter keeps the order a filter and then a stable sort would give it.
    let mut live: Vec<(&CooldownRow, &str)> = cooldowns
        .iter()
        .filter(|c| c.until > now && members.iter().any(|m| m.account_id == c.account_id))
        .filter_map(|c| c.scope.model().map(|model| (c, model)))
        .collect();
    live.sort_by(|(a, _), (b, _)| a.account_id.cmp(&b.account_id));
    let processed = rules::process(&key.model_rules, key.model_rules_mode, &pool.model_rules);
    let rule_rows: Vec<RuleEntry> = processed
        .iter()
        .map(|entry| {
            // A denied model is never routed, so its cooldowns say nothing about the key.
            let matching = if entry.rule.not {
                Vec::new()
            } else {
                live.iter()
                    .filter(|(_, model)| pattern::matches(&entry.rule.pattern, model))
                    .map(|(c, _)| cooldown_json(c))
                    .collect()
            };
            RuleEntry {
                level: entry.level.as_str().to_string(),
                pattern: entry.rule.pattern.clone(),
                not: entry.rule.not,
                cooldowns: matching,
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
