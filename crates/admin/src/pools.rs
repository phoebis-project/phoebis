//! Pool routes: the record every field of which the spec's pool table names, and its membership.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_keepalive::KeepalivePolicy;
use phoebis_relay::CacheTtlPolicy;
use phoebis_store::{OveragePolicy, PoolMember, PoolRow, StoreError, Strategy, SubStrategy};
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
    select_below_utilization: Option<Option<f64>>,
    #[serde(default, deserialize_with = "double_option")]
    default_allowed_models: Option<Option<Vec<String>>>,
    #[serde(default)]
    keepalive: Option<Value>,
    #[serde(default)]
    cache_ttl: Option<Value>,
    #[serde(default, deserialize_with = "double_option")]
    cache_diagnostics: Option<Option<bool>>,
    #[serde(default)]
    body_capture_hours: Option<u32>,
    #[serde(default)]
    members: Option<Vec<MemberInput>>,
    #[serde(default)]
    prefer_subscription: Option<bool>,
    #[serde(default)]
    api_key_sub_strategy: Option<String>,
}

/// Tells an absent field from an explicit `null`, so a `PATCH` can clear a nullable column.
fn double_option<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// A PATCH's `keepalive` names only the fields the operator touched, and the struct fills the rest with
/// defaults on parse; overlaying it on the row's stored policy first keeps every untouched field at its
/// current value instead of resetting it. A non-object `incoming` is passed through unchanged so `parse`'s own
/// shape check rejects it.
fn merged_keepalive(row: &PoolRow, incoming: &Value) -> Value {
    let Value::Object(patch) = incoming else {
        return incoming.clone();
    };
    let mut merged = serde_json::to_value(KeepalivePolicy::from_json(&row.keepalive_json))
        .unwrap_or_else(|_| json!({}));
    if let Value::Object(base) = &mut merged {
        for (key, value) in patch {
            base.insert(key.clone(), value.clone());
        }
    }
    merged
}

/// Mirrors `merged_keepalive`: a PATCH's `cache_ttl` names only the fields the operator touched, and the
/// struct fills the rest with defaults on parse.
fn merged_cache_ttl(row: &PoolRow, incoming: &Value) -> Value {
    let Value::Object(patch) = incoming else {
        return incoming.clone();
    };
    let mut merged = serde_json::to_value(CacheTtlPolicy::from_json(&row.cache_ttl_json))
        .unwrap_or_else(|_| json!({}));
    if let Value::Object(base) = &mut merged {
        for (key, value) in patch {
            base.insert(key.clone(), value.clone());
        }
    }
    merged
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
            "earliest_refresh" => Strategy::EarliestRefresh,
            "least_utilized" => Strategy::LeastUtilized,
            "priority_random" => Strategy::PriorityRandom,
            "round_robin" => Strategy::RoundRobin,
            _ => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "strategy must be earliest_refresh, least_utilized, priority_random, or round_robin",
                ))
            }
        };
    }
    if let Some(sub) = &input.api_key_sub_strategy {
        row.api_key_sub_strategy = match sub.as_str() {
            "priority_random" => SubStrategy::PriorityRandom,
            "round_robin" => SubStrategy::RoundRobin,
            _ => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "api_key_sub_strategy must be priority_random or round_robin",
                ))
            }
        };
    }
    // The flag is the operator's answer and nothing else writes it: a sample-driven strategy orders by kind
    // whatever it holds, and the pool answers both values so a strategy the operator tries out and takes back
    // leaves the preference where they left it.
    if let Some(prefer) = input.prefer_subscription {
        row.prefer_subscription = prefer;
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
    if let Some(threshold) = input.select_below_utilization {
        if threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "select_below_utilization is between 0 and 1",
            ));
        }
        row.select_below_utilization = threshold;
    }
    if let Some(list) = &input.default_allowed_models {
        row.default_allowed_models = list
            .as_ref()
            .map(|l| l.iter().map(|m| phoebis_store::canonical_id(m)).collect());
    }
    if let Some(keepalive) = &input.keepalive {
        let merged = merged_keepalive(row, keepalive);
        let policy = match KeepalivePolicy::parse(&merged) {
            Ok(policy) => policy,
            Err(error) => return Err(failure(StatusCode::BAD_REQUEST, &error.to_string())),
        };
        row.keepalive_json = serde_json::to_string(&policy).unwrap_or_else(|_| "{}".into());
    }
    if let Some(cache_ttl) = &input.cache_ttl {
        let mut merged = merged_cache_ttl(row, cache_ttl);
        if let Some(models) = merged.get_mut("models").and_then(Value::as_array_mut) {
            for entry in models.iter_mut() {
                if let Some(model) = entry
                    .get("model")
                    .and_then(Value::as_str)
                    .map(phoebis_store::canonical_id)
                {
                    entry["model"] = Value::String(model);
                }
            }
        }
        let policy = match CacheTtlPolicy::parse(&merged) {
            Ok(policy) => policy,
            Err(error) => return Err(failure(StatusCode::BAD_REQUEST, &error.to_string())),
        };
        row.cache_ttl_json = serde_json::to_string(&policy).unwrap_or_else(|_| "{}".into());
    }
    if let Some(diagnostics) = input.cache_diagnostics {
        row.cache_diagnostics = diagnostics;
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
    // Checked over the whole row rather than over the edited field alone, so lowering affinity under an
    // enabled policy is refused exactly as enabling under a short affinity is.
    if let Err(error) =
        KeepalivePolicy::from_json(&row.keepalive_json).validate(row.affinity_ttl_seconds)
    {
        return Err(failure(StatusCode::BAD_REQUEST, &error.to_string()));
    }
    Ok(())
}

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.pools() {
        Ok(rows) => {
            let mut out = Vec::new();
            for row in rows {
                match pool_json(&state, &row).await {
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
    let (Some(id), Some(_)) = (&input.id, &input.label) else {
        return failure(StatusCode::BAD_REQUEST, "id and label are required");
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
        strategy: Strategy::LeastUtilized,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        default_allowed_models: None,
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        prefer_subscription: false,
        api_key_sub_strategy: SubStrategy::PriorityRandom,
        cache_ttl_json: "{}".into(),
        cache_diagnostics: None,
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
                // The pool was never published, so nothing needs a reload here; but a rollback that fails
                // leaves an orphan row that only a later, unrelated reload would surface.
                match state.store.delete_pool(&row.id, ACTOR) {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!(pool = %row.id, %error, "a pool created for a missing member could not be rolled back")
                    }
                }
                return failure(StatusCode::NOT_FOUND, &format!("account {id} not found"));
            }
            Err(error) => return store_failure(error),
        }
    }
    if let Err(refusal) = reload_or_refuse(&state).await {
        return refusal;
    }
    match pool_json(&state, &row).await {
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
    match pool_json(&state, &row).await {
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
    match pool_json(&state, &row).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => store_failure(e),
    }
}

pub(crate) async fn pool_json(state: &AdminState, row: &PoolRow) -> Result<Value, StoreError> {
    let members = state.store.pool_members(&row.id)?;
    let keys: Vec<Value> = state
        .store
        .api_keys()?
        .into_iter()
        .filter(|k| k.pool_id == row.id)
        .map(|k| json!({ "id": k.id, "label": k.label, "prefix": k.prefix, "enabled": k.enabled }))
        .collect();
    let now = (state.clock)();
    let hour = phoebis_ledger::hour_of(now);
    let pool_id = row.id.clone();
    let (day, traffic) = state
        .ledger
        .db()
        .read(move |c| {
            Ok((
                phoebis_ledger::request_totals(c, "pool_id", hour - 24 * 3_600)?,
                phoebis_ledger::pool_traffic(c, &pool_id, hour - 23 * 3_600, hour + 3_600, 3_600)?,
            ))
        })
        .await
        .unwrap_or_default();
    let cooldowns = state.router.cooldowns().await.unwrap_or_default();
    let overages = state.router.overages().await.unwrap_or_default();
    let pins = state.router.pins().await.unwrap_or_default();
    let key_ids: Vec<String> = keys
        .iter()
        .map(|k| k["id"].as_str().unwrap_or_default().to_string())
        .collect();
    let active_pins = pins.iter().filter(|p| key_ids.contains(&p.key_id)).count();
    let accounts = state.store.accounts()?;
    // Why routing would refuse a member, mirroring the selector's `obstacle`: not enabled or not ready/mid-refresh,
    // an account-wide cooldown, or overage the pool's policy admits no substitute for. `None` is eligible.
    let ineligible = |m: &PoolMember| -> Option<&'static str> {
        let ready = accounts
            .iter()
            .find(|a| a.id == m.account_id)
            .is_some_and(|a| a.enabled && a.state == phoebis_store::AccountState::Ready);
        if !ready {
            return Some("unavailable");
        }
        if cooldowns
            .iter()
            .any(|c| c.account_id == m.account_id && c.scope == "account" && c.until > now)
        {
            return Some("cooldown");
        }
        if row.overage_policy == OveragePolicy::Never
            && overages
                .iter()
                .any(|(id, e)| *id == m.account_id && e.in_use)
        {
            return Some("overage");
        }
        None
    };
    let eligible = |m: &PoolMember| ineligible(m).is_none();
    let status = if !row.enabled {
        "disabled"
    } else if members.is_empty() || !members.iter().any(eligible) {
        "exhausted"
    } else if !members.iter().all(eligible) {
        "degraded"
    } else {
        "active"
    };
    Ok(json!({
        "id": row.id, "label": row.label, "enabled": row.enabled,
        "strategy": row.strategy.as_str(), "api_key_sub_strategy": row.api_key_sub_strategy.as_str(),
        "prefer_subscription": row.prefer_subscription, "prefer_subscription_effective": row.kind_ordered(),
        "affinity_ttl_seconds": row.affinity_ttl_seconds, "overage_policy": row.overage_policy.as_str(),
        "migrate_above_utilization": row.migrate_above_utilization, "select_below_utilization": row.select_below_utilization,
        "default_allowed_models": row.default_allowed_models,
        "keepalive": KeepalivePolicy::from_json(&row.keepalive_json),
        "cache_ttl": CacheTtlPolicy::from_json(&row.cache_ttl_json), "cache_diagnostics": row.cache_diagnostics,
        "body_capture_hours": row.body_capture_hours, "created_at": row.created_at, "updated_at": row.updated_at,
        "members": members.iter().map(|m| json!({ "account_id": m.account_id, "priority": m.priority, "eligible": ineligible(m).unwrap_or("eligible") })).collect::<Vec<_>>(),
        "keys": keys,
        "status": status,
        "requests_24h": day.get(&row.id).map_or(0, |t| t.requests),
        "cost_24h": day.get(&row.id).map_or(0.0, |t| t.cost_usd),
        "active_pins": active_pins,
        "traffic_24h": traffic,
    }))
}

pub(crate) async fn traffic(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match state.store.pool(&id) {
        Ok(Some(_)) => {}
        Ok(None) => return failure(StatusCode::NOT_FOUND, "pool not found"),
        Err(e) => return store_failure(e),
    }
    let now = (state.clock)();
    let (from, to) = match phoebis_ledger::parse_range(q.get("range").map(String::as_str), now) {
        Ok(r) => r,
        Err(m) => return failure(StatusCode::BAD_REQUEST, &m),
    };
    let bucket = phoebis_ledger::bucket_seconds(from, to);
    let pool_id = id.clone();
    match state
        .ledger
        .db()
        .read(move |c| {
            phoebis_ledger::pool_traffic(
                c,
                &pool_id,
                phoebis_ledger::hour_of(from),
                phoebis_ledger::hour_ceil(to),
                bucket,
            )
        })
        .await
    {
        Ok(buckets) => Json(json!({ "pool_id": id, "from": from, "to": to, "bucket_seconds": bucket, "buckets": buckets })).into_response(),
        Err(e) => crate::analytics::ledger_failure(e),
    }
}
