//! Pool routes: the record every field of which the spec's pool table names, and its membership.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_store::{OveragePolicy, PoolRow, StoreError, Strategy};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::{failure, reload_or_refuse, store_failure};
use crate::AdminState;

const ACTOR: &str = "admin";

#[derive(serde::Deserialize)]
struct MemberInput {
    account_id: String,
    #[serde(default)]
    priority: u32,
}

#[derive(serde::Deserialize, Default)]
struct PoolInput {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    affinity_ttl_seconds: Option<u64>,
    #[serde(default)]
    overage_policy: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    migrate_above_utilization: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    default_allowed_models: Option<Option<Vec<String>>>,
    #[serde(default)]
    keepalive: Option<Value>,
    #[serde(default)]
    body_capture_hours: Option<u32>,
    #[serde(default)]
    members: Option<Vec<MemberInput>>,
}

/// Tells an absent field from an explicit `null`, so a `PATCH` can clear a nullable column.
fn double_option<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

pub(crate) fn valid_slug(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || b.is_ascii_digit() || (b == b'-' && i > 0))
}

/// Applies `input` over `row`, refusing every value the spec's pool table rules out.
// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn apply(state: &AdminState, row: &mut PoolRow, input: &PoolInput) -> Result<(), Response> {
    if let Some(label) = &input.label {
        row.label = label.clone();
    }
    if let Some(enabled) = input.enabled {
        row.enabled = enabled;
    }
    if let Some(strategy) = &input.strategy {
        row.strategy = match strategy.as_str() {
            "priority_random" => Strategy::PriorityRandom,
            "least_utilized" => Strategy::LeastUtilized,
            _ => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "strategy must be priority_random or least_utilized",
                ))
            }
        };
    }
    if let Some(ttl) = input.affinity_ttl_seconds {
        if ttl != 0 && ttl < 60 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "affinity_ttl_seconds is 0 or at least 60",
            ));
        }
        row.affinity_ttl_seconds = ttl;
    }
    if let Some(policy) = &input.overage_policy {
        row.overage_policy = match policy.as_str() {
            "prefer_plan" => OveragePolicy::PreferPlan,
            "allow" => OveragePolicy::Allow,
            "never" => OveragePolicy::Never,
            _ => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "overage_policy must be prefer_plan, allow, or never",
                ))
            }
        };
    }
    if let Some(threshold) = input.migrate_above_utilization {
        if threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "migrate_above_utilization is between 0 and 1",
            ));
        }
        row.migrate_above_utilization = threshold;
    }
    if let Some(list) = &input.default_allowed_models {
        row.default_allowed_models = list
            .as_ref()
            .map(|l| l.iter().map(|m| phoebis_store::canonical_id(m)).collect());
    }
    if let Some(keepalive) = &input.keepalive {
        if !keepalive.is_object() {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "keepalive must be an object",
            ));
        }
        row.keepalive_json = keepalive.to_string();
    }
    if let Some(hours) = input.body_capture_hours {
        if hours > state.body_capture_max_hours {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "body_capture_hours exceeds stats.body_capture_max_hours",
            ));
        }
        row.body_capture_hours = hours;
    }
    Ok(())
}

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.pools() {
        Ok(rows) => {
            let mut out = Vec::new();
            for row in rows {
                match pool_json(&state, &row) {
                    Ok(v) => out.push(v),
                    Err(e) => return store_failure(e),
                }
            }
            Json(json!({ "pools": out })).into_response()
        }
        Err(error) => store_failure(error),
    }
}

pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let Ok(input) = serde_json::from_slice::<PoolInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let (Some(id), Some(_), Some(_)) = (&input.id, &input.label, &input.strategy) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "id, label, and strategy are required",
        );
    };
    if !valid_slug(id) {
        return failure(
            StatusCode::BAD_REQUEST,
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        );
    }
    if let Ok(Some(_)) = state.store.pool(id) {
        return failure(StatusCode::CONFLICT, "a pool with this id exists");
    }
    let now = (state.clock)();
    let mut row = PoolRow {
        id: id.clone(),
        label: String::new(),
        enabled: true,
        strategy: Strategy::PriorityRandom,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        default_allowed_models: None,
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        created_at: now,
        updated_at: now,
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    if let Err(error) = state.store.insert_pool(&row, ACTOR) {
        return store_failure(error);
    }
    if let Some(members) = &input.members {
        match state.store.set_pool_members(
            &row.id,
            &members
                .iter()
                .map(|m| (m.account_id.clone(), m.priority))
                .collect::<Vec<_>>(),
            ACTOR,
        ) {
            Ok(()) => {}
            Err(StoreError::AccountNotFound(id)) => {
                let _ = state.store.delete_pool(&row.id, ACTOR);
                return failure(StatusCode::NOT_FOUND, &format!("account {id} not found"));
            }
            Err(error) => return store_failure(error),
        }
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match pool_json(&state, &row) {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<PoolInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let mut row = match state.store.pool(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "pool not found"),
        Err(e) => return store_failure(e),
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    if let Err(error) = state.store.update_pool(&row, ACTOR) {
        return store_failure(error);
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match pool_json(&state, &row) {
        Ok(v) => Json(v).into_response(),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn remove(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_pool(&id, ACTOR) {
        Ok(()) => match reload_or_refuse(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(refusal) => refusal,
        },
        Err(StoreError::PoolInUse(_)) => failure(StatusCode::CONFLICT, "pool still has keys"),
        Err(StoreError::PoolNotFound(_)) => failure(StatusCode::NOT_FOUND, "pool not found"),
        Err(error) => store_failure(error),
    }
}

pub(crate) async fn set_members(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    #[derive(serde::Deserialize)]
    struct Input {
        members: Vec<MemberInput>,
    }
    let Ok(input) = serde_json::from_slice::<Input>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must carry a members list");
    };
    let row = match state.store.pool(&id) {
        Ok(Some(row)) => row,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "pool not found"),
        Err(e) => return store_failure(e),
    };
    match state.store.set_pool_members(
        &id,
        &input
            .members
            .iter()
            .map(|m| (m.account_id.clone(), m.priority))
            .collect::<Vec<_>>(),
        ACTOR,
    ) {
        Ok(()) => {}
        Err(StoreError::AccountNotFound(id)) => {
            return failure(StatusCode::NOT_FOUND, &format!("account {id} not found"))
        }
        Err(error) => return store_failure(error),
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match pool_json(&state, &row) {
        Ok(v) => Json(v).into_response(),
        Err(e) => store_failure(e),
    }
}

pub(crate) fn pool_json(state: &AdminState, row: &PoolRow) -> Result<Value, StoreError> {
    let members = state.store.pool_members(&row.id)?;
    let keys: Vec<Value> = state
        .store
        .api_keys()?
        .into_iter()
        .filter(|k| k.pool_id == row.id)
        .map(|k| json!({ "id": k.id, "label": k.label, "prefix": k.prefix, "enabled": k.enabled }))
        .collect();
    Ok(json!({
        "id": row.id, "label": row.label, "enabled": row.enabled,
        "strategy": row.strategy.as_str(), "affinity_ttl_seconds": row.affinity_ttl_seconds, "overage_policy": row.overage_policy.as_str(),
        "migrate_above_utilization": row.migrate_above_utilization, "default_allowed_models": row.default_allowed_models,
        "keepalive": serde_json::from_str::<Value>(&row.keepalive_json).unwrap_or(Value::Object(Default::default())),
        "body_capture_hours": row.body_capture_hours, "created_at": row.created_at, "updated_at": row.updated_at,
        "members": members.iter().map(|m| json!({ "account_id": m.account_id, "priority": m.priority })).collect::<Vec<_>>(),
        "keys": keys,
    }))
}
