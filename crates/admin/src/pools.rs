//! Pool routes: the record every field of which the spec's pool table names, and its membership.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_keepalive::KeepalivePolicy;
use phoebis_relay::CacheTtlPolicy;
use phoebis_routing::{MemberSpend, OverageEvidence, RegistrySnapshot};
use phoebis_store::{
    AccountRow, Affinity, AffinityMode, ApiKeyRow, CooldownRow, Failover, MemberWrite,
    OveragePolicy, PinRow, PoolMember, PoolRow, SessionPlacement, Shedding, StoreError, Strategy,
    SubagentPlacement, WarmSessions,
};
use serde_json::{json, Value};

use crate::routes::{double_option, failure, reload_or_refuse, store_failure, CreatedId};
use crate::rules::{parse_rules, RuleInput};
use crate::AdminState;

const ACTOR: &str = "admin";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberInput {
    account_id: String,
    #[serde(default)]
    priority: u32,
    #[serde(default)]
    budget_daily_usd: Option<f64>,
    #[serde(default)]
    budget_monthly_usd: Option<f64>,
}

/// Turns the wire members into store writes, refusing an account named twice and a non-positive budget before any
/// write lands; a member list replaces the pool's whole membership, so an absent budget clears it. A subscription
/// member's budgets bound its metered spend, the only spend of its that costs money.
// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
fn member_writes(members: &[MemberInput]) -> Result<Vec<MemberWrite>, Response> {
    // Ahead of every other check, the store's existence check included, so a list naming an unknown account
    // twice is refused for the repeat.
    let mut seen = HashSet::new();
    if let Some(repeat) = members.iter().find(|m| !seen.insert(m.account_id.as_str())) {
        return Err(failure(
            StatusCode::CONFLICT,
            &format!("members name account {} twice", repeat.account_id),
        ));
    }
    members
        .iter()
        .map(|m| {
            if m.budget_daily_usd.is_some_and(|b| b <= 0.0) {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "budget_daily_usd is above 0",
                ));
            }
            if m.budget_monthly_usd.is_some_and(|b| b <= 0.0) {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "budget_monthly_usd is above 0",
                ));
            }
            Ok(MemberWrite {
                account_id: m.account_id.clone(),
                priority: m.priority,
                budget_daily_usd: m.budget_daily_usd,
                budget_monthly_usd: m.budget_monthly_usd,
            })
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    model_rules: Option<Vec<RuleInput>>,
    #[serde(default)]
    keepalive: Option<Value>,
    #[serde(default)]
    cache_ttl: Option<Value>,
    #[serde(default, deserialize_with = "double_option")]
    cache_diagnostics: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    auto_mode_server: Option<Option<Value>>,
    #[serde(default)]
    body_capture_hours: Option<u32>,
    #[serde(default)]
    members: Option<Vec<MemberInput>>,
    #[serde(default)]
    prefer_subscription: Option<bool>,
    #[serde(default)]
    shed_share: Option<f64>,
    #[serde(default)]
    burn_window_seconds: Option<u64>,
    #[serde(default)]
    source_dwell_seconds: Option<u64>,
    #[serde(default)]
    migrate_when_spent: Option<bool>,
    #[serde(default)]
    warm_sessions: Option<String>,
    #[serde(default)]
    retry_in_place_seconds: Option<u64>,
    #[serde(default)]
    in_flight_cap: Option<u32>,
    #[serde(default)]
    pace_floor: Option<f64>,
    #[serde(default)]
    overage_move_grace_minutes: Option<u32>,
    #[serde(default)]
    wind_down_minutes: Option<u32>,
    #[serde(default)]
    affinity_mode: Option<String>,
    #[serde(default)]
    subagent_placement: Option<String>,
    #[serde(default)]
    session_placement: Option<String>,
    #[serde(default)]
    min_shared_prefix_tokens: Option<u64>,
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

/// Mirrors `merged_keepalive`, except `plan` and `metered` merge one level deeper: a PATCH naming
/// `{"metered": {"main": "5m"}}` overlays that field alone, leaving the set's other three where they stood
/// rather than falling back to the set's own defaults.
fn merged_cache_ttl(row: &PoolRow, incoming: &Value) -> Value {
    let Value::Object(patch) = incoming else {
        return incoming.clone();
    };
    let mut merged = serde_json::to_value(CacheTtlPolicy::from_json(&row.cache_ttl_json))
        .unwrap_or_else(|_| json!({}));
    if let Value::Object(base) = &mut merged {
        for (key, value) in patch {
            if let (true, Value::Object(set_patch)) =
                (matches!(key.as_str(), "metered" | "plan"), value)
            {
                if let Some(Value::Object(set_base)) = base.get_mut(key) {
                    for (field, field_value) in set_patch {
                        set_base.insert(field.clone(), field_value.clone());
                    }
                    continue;
                }
            }
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
            "pace" => Strategy::Pace,
            "priority" => Strategy::Priority,
            "round_robin" => Strategy::RoundRobin,
            _ => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "strategy must be pace, priority, or round_robin",
                ))
            }
        };
    }
    // The flag is the operator's answer and nothing else writes it: a pool orders by kind only while this is
    // on, so a strategy the operator tries out and takes back leaves the preference where they left it.
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
    if let Some(list) = &input.model_rules {
        row.model_rules = parse_rules(list)?;
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
        for set in ["metered", "plan"] {
            if let Some(models) = merged
                .get_mut(set)
                .and_then(|s| s.get_mut("models"))
                .and_then(Value::as_array_mut)
            {
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
        }
        let policy = match CacheTtlPolicy::parse(&merged) {
            Ok(policy) => policy,
            Err(error) => return Err(failure(StatusCode::BAD_REQUEST, &error.to_string())),
        };
        row.cache_ttl_json = serde_json::to_string(&policy).unwrap_or_else(|_| "{}".into());
    }
    if let Some(switch) = input.cache_diagnostics {
        // A null names neither state of the switch, so it is refused rather than read as one of them.
        row.cache_diagnostics = match switch {
            Some(on) => on,
            None => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "cache_diagnostics must be true or false",
                ))
            }
        };
    }
    if let Some(value) = &input.auto_mode_server {
        row.auto_mode_server = match value {
            Some(Value::Bool(on)) => *on,
            _ => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "auto_mode_server must be true or false",
                ))
            }
        };
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
    if let Some(share) = input.shed_share {
        if !(0.0..=1.0).contains(&share) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "shed_share is between 0 and 1",
            ));
        }
        row.shedding.shed_share = share;
    }
    if let Some(seconds) = input.burn_window_seconds {
        if seconds < 60 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "burn_window_seconds is at least 60",
            ));
        }
        row.shedding.burn_window_seconds = seconds;
    }
    if let Some(seconds) = input.source_dwell_seconds {
        if seconds < 60 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "source_dwell_seconds is at least 60",
            ));
        }
        row.shedding.source_dwell_seconds = seconds;
    }
    if let Some(spent) = input.migrate_when_spent {
        row.shedding.migrate_when_spent = spent;
    }
    if let Some(mode) = &input.warm_sessions {
        row.shedding.warm_sessions = match WarmSessions::parse(mode) {
            Some(mode) => mode,
            None => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "warm_sessions must be always, never, or when_needed",
                ))
            }
        };
    }
    if let Some(seconds) = input.retry_in_place_seconds {
        if seconds > 300 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "retry_in_place_seconds is 0 to 300",
            ));
        }
        row.failover.retry_in_place_seconds = seconds;
    }
    if let Some(cap) = input.in_flight_cap {
        if cap < 1 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "in_flight_cap is at least 1",
            ));
        }
        row.failover.in_flight_cap = cap;
    }
    if let Some(floor) = input.pace_floor {
        if !(0.0..=1.0).contains(&floor) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "pace_floor is between 0 and 1",
            ));
        }
        row.failover.pace_floor = floor;
    }
    if let Some(minutes) = input.overage_move_grace_minutes {
        if minutes > 300 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "overage_move_grace_minutes is 0 to 300",
            ));
        }
        row.failover.overage_move_grace_minutes = minutes;
    }
    if let Some(minutes) = input.wind_down_minutes {
        if minutes > 10_080 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "wind_down_minutes is at most 10080 (one week)",
            ));
        }
        row.failover.wind_down_minutes = minutes;
    }
    if let Some(mode) = &input.affinity_mode {
        row.affinity.mode = match AffinityMode::parse(mode) {
            Some(mode) => mode,
            None => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "affinity_mode must be always or when_cached",
                ))
            }
        };
    }
    if let Some(placement) = &input.subagent_placement {
        row.affinity.subagent_placement = match SubagentPlacement::parse(placement) {
            Some(placement) => placement,
            None => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "subagent_placement must be by_cohort, by_strategy, or with_main",
                ))
            }
        };
    }
    if let Some(placement) = &input.session_placement {
        row.affinity.session_placement = match SessionPlacement::parse(placement) {
            Some(placement) => placement,
            None => {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "session_placement must be by_strategy or by_warm_prefix",
                ))
            }
        };
    }
    if let Some(tokens) = input.min_shared_prefix_tokens {
        row.affinity.min_shared_prefix_tokens = tokens;
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
    let context = match pool_context(&state).await {
        Ok(context) => context,
        Err(error) => return store_failure(error),
    };
    let mut out = Vec::with_capacity(context.pools.len());
    for row in &context.pools {
        out.push(pool_json_from(&state, &context, row).await);
    }
    Json(json!({ "pools": out })).into_response()
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
        strategy: Strategy::Pace,
        affinity_ttl_seconds: 3600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        model_rules: Vec::new(),
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        prefer_subscription: true,
        shedding: Shedding::default(),
        failover: Failover::default(),
        cache_ttl_json: "{}".into(),
        cache_diagnostics: false,
        auto_mode_server: true,
        affinity: Affinity::default(),
        created_at: now,
        updated_at: now,
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    let writes = match &input.members {
        Some(members) => match member_writes(members) {
            Ok(writes) => Some(writes),
            Err(refusal) => return refusal,
        },
        None => None,
    };
    match state
        .store
        .insert_pool_with_members(&row, writes.as_deref(), ACTOR)
    {
        Ok(()) => {}
        // The row and its members are one transaction, so a refused member leaves no pool to name.
        Err(StoreError::AccountNotFound(id)) => {
            return failure(StatusCode::NOT_FOUND, &format!("account {id} not found"))
        }
        Err(error) => return store_failure(error),
    }
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = publish_created(&state, &row).await;
    response.extensions_mut().insert(CreatedId(row.id));
    response
}

async fn publish_created(state: &AdminState, row: &PoolRow) -> Response {
    if let Err(refusal) = reload_or_refuse(state).await {
        return refusal;
    }
    match pool_json(state, row).await {
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
    let read_at = row.updated_at;
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    let writes = match &input.members {
        Some(members) => match member_writes(members) {
            Ok(writes) => Some(writes),
            Err(refusal) => return refusal,
        },
        None => None,
    };
    match state
        .store
        .update_pool_with_members(&row, read_at, writes.as_deref(), ACTOR)
    {
        Ok(()) => {}
        Err(StoreError::Stale) => {
            return failure(
                StatusCode::CONFLICT,
                "the pool changed while it was being edited; reload and try again",
            )
        }
        Err(StoreError::PoolNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "pool not found")
        }
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
    let writes = match member_writes(&input.members) {
        Ok(writes) => writes,
        Err(refusal) => return refusal,
    };
    match state.store.set_pool_members(&id, &writes, ACTOR) {
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

/// One member row. Its own function, beside the pool's own `json!` call, so one row's worth of fields reads
/// clearly on its own rather than nested inline inside the pool's larger literal.
fn member_json(
    m: &PoolMember,
    accounts: &[phoebis_store::AccountRow],
    spends: &[phoebis_routing::MemberSpend],
    eligible: &str,
    now: i64,
) -> Value {
    let kind = accounts
        .iter()
        .find(|a| a.id == m.account_id)
        .map(|a| match a.kind {
            phoebis_store::AccountKind::ApiKey => "api_key",
            phoebis_store::AccountKind::Oauth => "oauth",
        });
    let rows = crate::routes::spend_rows(spends, &m.pool_id, &m.account_id);
    let reports: Vec<Value> = rows
        .iter()
        .filter(|s| m.budget(s.period).is_some())
        .map(|s| json!({ "period": s.period.as_str(), "spent_usd": s.spent_usd, "period_end": s.period_end }))
        .collect();
    json!({
        "account_id": m.account_id, "priority": m.priority, "eligible": eligible, "kind": kind,
        "budget_daily_usd": m.budget_daily_usd, "budget_monthly_usd": m.budget_monthly_usd,
        "budget_until": phoebis_routing::budget_deadline(m, &rows, now), "spends": reports,
    })
}

/// What every pool row reads from the registry, the ledger and the router, fetched once per list call.
pub(crate) struct PoolContext {
    accounts: Vec<AccountRow>,
    cooldowns: Vec<CooldownRow>,
    /// The day's request totals per pool.
    day: BTreeMap<String, phoebis_ledger::Totals>,
    keys: Vec<ApiKeyRow>,
    members: Vec<PoolMember>,
    now: i64,
    overages: Vec<(String, OverageEvidence)>,
    pins: Vec<PinRow>,
    pools: Vec<PoolRow>,
    snapshot: Arc<RegistrySnapshot>,
    spends: Vec<MemberSpend>,
}

/// One registry read for the pools, members, keys and accounts, one ledger read for the day's totals and one ask
/// of the router for each of its tables, in the shape of the account list's context.
pub(crate) async fn pool_context(state: &AdminState) -> Result<PoolContext, StoreError> {
    let registry = state.store.load_registry()?;
    let now = (state.clock)();
    let hour = phoebis_ledger::hour_of(now);
    let day = state
        .ledger
        .db()
        .read(move |c| phoebis_ledger::request_totals(c, "pool_id", hour - 24 * 3_600))
        .await
        .unwrap_or_default();
    Ok(PoolContext {
        accounts: registry.accounts,
        cooldowns: state.router.cooldowns().await.unwrap_or_default(),
        day,
        keys: registry.keys,
        members: registry.members,
        now,
        overages: state.router.overages().await.unwrap_or_default(),
        pins: state.router.pins().await.unwrap_or_default(),
        pools: registry.pools,
        snapshot: state.router.snapshot(),
        spends: state.router.member_spends().await.unwrap_or_default(),
    })
}

/// One pool over a context built for it alone, for the routes that answer a single pool.
pub(crate) async fn pool_json(state: &AdminState, row: &PoolRow) -> Result<Value, StoreError> {
    let context = pool_context(state).await?;
    Ok(pool_json_from(state, &context, row).await)
}

/// One pool row from the shared context; the traffic and the moves stay per pool, since they are ledger reads over
/// that pool's own id and accounts and take no store lock.
pub(crate) async fn pool_json_from(
    state: &AdminState,
    context: &PoolContext,
    row: &PoolRow,
) -> Value {
    let mut members: Vec<PoolMember> = context
        .members
        .iter()
        .filter(|m| m.pool_id == row.id)
        .cloned()
        .collect();
    members.sort_by(|a, b| (a.priority, &a.account_id).cmp(&(b.priority, &b.account_id)));
    let keys: Vec<Value> = context
        .keys
        .iter()
        .filter(|k| k.pool_id == row.id)
        .map(|k| json!({ "id": k.id, "label": k.label, "prefix": k.prefix, "enabled": k.enabled }))
        .collect();
    let now = context.now;
    let hour = phoebis_ledger::hour_of(now);
    let day = &context.day;
    let traffic = phoebis_ledger::pool_traffic(
        state.ledger.db(),
        &row.id,
        hour - 23 * 3_600,
        hour + 3_600,
        3_600,
    )
    .await
    .unwrap_or_default();
    // A memberless pool has no account to attribute a move against, so this skips the read outright rather
    // than spend one on a query `pin_migrations_by_reason` would answer empty anyway.
    let account_ids: Vec<String> = members.iter().map(|m| m.account_id.clone()).collect();
    let moves = if account_ids.is_empty() {
        std::collections::BTreeMap::new()
    } else {
        state
            .ledger
            .db()
            .read(move |c| {
                phoebis_ledger::pin_migrations_by_reason(c, now - 3_600_000, now, &account_ids)
            })
            .await
            .unwrap_or_default()
    };
    let (cooldowns, overages, pins) = (&context.cooldowns, &context.overages, &context.pins);
    let key_ids: Vec<String> = keys
        .iter()
        .map(|k| k["id"].as_str().unwrap_or_default().to_string())
        .collect();
    let active_pins = pins.iter().filter(|p| key_ids.contains(&p.key_id)).count();
    let accounts = &context.accounts;
    let spends = &context.spends;
    let snapshot = &context.snapshot;
    let fallback_ms = phoebis_routing::seconds_ms(state.settings.cooldown_fallback_seconds);
    // Why routing would refuse a member, mirroring the selector's `obstacle` in the order it checks them: not
    // enabled or not ready, a subscription past its saved end, an account-wide cooldown, a budget the member's
    // own spend has reached, or overage the pool's policy admits no substitute for. `None` is eligible. An
    // ended subscription reads `unavailable` as a paused account does: both stay skipped until an operator acts.
    let ineligible = |m: &PoolMember| -> Option<&'static str> {
        let ready = accounts
            .iter()
            .find(|a| a.id == m.account_id)
            .is_some_and(|a| a.enabled && a.state == phoebis_store::AccountState::Ready);
        let ended = snapshot
            .term(&m.account_id)
            .is_some_and(|term| phoebis_routing::term::ended(term, now));
        if !ready || ended {
            return Some("unavailable");
        }
        if cooldowns
            .iter()
            .any(|c| c.account_id == m.account_id && c.scope == "account" && c.until > now)
        {
            return Some("cooldown");
        }
        let rows = crate::routes::spend_rows(spends, &row.id, &m.account_id);
        if phoebis_routing::budget_deadline(m, &rows, now).is_some() {
            return Some("budget");
        }
        if row.overage_policy == OveragePolicy::Never
            && overages
                .iter()
                .any(|(id, e)| *id == m.account_id && e.stands(now, fallback_ms))
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
    json!({
        "id": row.id, "label": row.label, "enabled": row.enabled,
        "strategy": row.strategy.as_str(),
        "prefer_subscription": row.prefer_subscription,
        "affinity_ttl_seconds": row.affinity_ttl_seconds, "overage_policy": row.overage_policy.as_str(),
        "affinity_mode": row.affinity.mode.as_str(), "subagent_placement": row.affinity.subagent_placement.as_str(),
        "session_placement": row.affinity.session_placement.as_str(), "min_shared_prefix_tokens": row.affinity.min_shared_prefix_tokens,
        "migrate_above_utilization": row.migrate_above_utilization, "select_below_utilization": row.select_below_utilization,
        "model_rules": row.model_rules,
        "shed_share": row.shedding.shed_share, "burn_window_seconds": row.shedding.burn_window_seconds,
        "source_dwell_seconds": row.shedding.source_dwell_seconds, "migrate_when_spent": row.shedding.migrate_when_spent,
        "warm_sessions": row.shedding.warm_sessions.as_str(),
        "retry_in_place_seconds": row.failover.retry_in_place_seconds, "in_flight_cap": row.failover.in_flight_cap,
        "pace_floor": row.failover.pace_floor, "overage_move_grace_minutes": row.failover.overage_move_grace_minutes,
        "wind_down_minutes": row.failover.wind_down_minutes,
        "moves_per_hour": moves.iter().map(|(reason, count)| (reason.clone(), *count as f64)).collect::<std::collections::BTreeMap<_, _>>(),
        "keepalive": KeepalivePolicy::from_json(&row.keepalive_json),
        "cache_ttl": CacheTtlPolicy::from_json(&row.cache_ttl_json), "cache_diagnostics": row.cache_diagnostics,
        "auto_mode_server": row.auto_mode_server,
        "body_capture_hours": row.body_capture_hours, "created_at": row.created_at, "updated_at": row.updated_at,
        "members": members.iter().map(|m| member_json(m, accounts, spends, ineligible(m).unwrap_or("eligible"), now)).collect::<Vec<_>>(),
        "keys": keys,
        "status": status,
        "requests_24h": day.get(&row.id).map_or(0, |t| t.requests),
        "cost_24h": day.get(&row.id).map_or(0.0, |t| t.cost_usd),
        "active_pins": active_pins,
        "traffic_24h": traffic,
    })
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
    match phoebis_ledger::pool_traffic(
        state.ledger.db(),
        &id,
        phoebis_ledger::hour_of(from),
        phoebis_ledger::hour_ceil(to),
        bucket,
    )
    .await
    {
        Ok(buckets) => Json(json!({ "pool_id": id, "from": from, "to": to, "bucket_seconds": bucket, "buckets": buckets })).into_response(),
        Err(e) => crate::analytics::ledger_failure(e),
    }
}
