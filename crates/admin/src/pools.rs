//! Pool routes: the record every field of which the spec's pool table names, and its membership.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_keepalive::{pool_fundings, CeilingInputs, KeepalivePolicy};
use phoebis_relay::{CacheTtl, CacheTtlPolicy};
use phoebis_routing::{AccountProjection, MemberSpend, Obstacle, RegistrySnapshot};
use phoebis_store::{
    stamp_token, AccountRow, AffinityMode, ApiKeyRow, AttachmentSubject, MemberWrite,
    OveragePolicy, PinRow, PoolMember, PoolRow, ReturnToPlan, SessionPlacement, StoreError,
    Strategy, SubagentPlacement, WarmSessions,
};
use serde_json::{json, Value};
use utoipa::ToSchema;

use crate::accounts::{check_slug, ACTOR};
use crate::guardrails::{attachment_refusal, distinct_ids, AttachmentIndex, GuardrailRef};
use crate::openapi::{query_refusal, ErrorBody, RegistryUnavailable, StorageFailure};
use crate::routes::{
    double_option, failure, fence_token, parse_object, pool, present, reload_or_no_content,
    reload_or_refuse, stale_edit, store_failure, AdminQuery, CreatedId,
};
use crate::rules::{parse_rules, RuleInput};
use crate::AdminState;

const DUPLICATE_ID: &str = "a pool with this id exists";
const STALE_EDIT: &str = "the pool changed while it was being edited, so reload and try again";

/// One member. On a subscription member a budget bounds its metered spend, overage and fast answers, and on a
/// usage-based seat its answers billed under `usage_based`.
#[derive(serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberInput {
    account_id: String,
    /// Default `0`.
    #[serde(default)]
    priority: u32,
    /// Above `0`, or absent for no cap.
    #[serde(default)]
    budget_daily_usd: Option<f64>,
    /// Above `0`, or absent for no cap.
    #[serde(default)]
    budget_monthly_usd: Option<f64>,
}

/// The whole membership list.
#[derive(serde::Deserialize, ToSchema)]
pub(crate) struct MembersInput {
    pub members: Vec<MemberInput>,
}

/// Turns the wire members into store writes, refusing an account named twice and a non-positive budget before any
/// write lands; a member list replaces the pool's whole membership, so an absent budget clears it. A subscription
/// member's budgets bound its metered spend, the only spend of its that costs money.
// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
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

/// The pool's fields. On a create `id` and `label` are required and every other field takes its default. On an edit
/// any subset but `id`, and `members`, when present, replaces the member list in the same call, as `PUT
/// /admin/pools/{id}/members` does: the settings and the members are written in one transaction, so a refused
/// member list writes neither. An unknown field in the body is refused.
#[derive(serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolInput {
    /// `[a-z0-9][a-z0-9-]{0,63}`.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    /// `pace`, `priority`, or `round_robin`.
    #[serde(default)]
    #[schema(example = "pace")]
    strategy: Option<String>,
    /// `0` or at least `60`, default `3600`.
    #[serde(default)]
    affinity_ttl_seconds: Option<u64>,
    /// `allow`, `never`, or `prefer_plan`, default `prefer_plan`.
    #[serde(default)]
    #[schema(example = "allow")]
    overage_policy: Option<String>,
    /// `0.5` to `1.0`, default `0.95`.
    #[serde(default)]
    window_ceiling: Option<f64>,
    /// An ordered list of `{pattern, not}`, at most 256.
    #[serde(default)]
    model_rules: Option<Vec<RuleInput>>,
    /// A partial policy of the four fields, merged onto the pool's stored policy, which a create starts at the
    /// defaults, bounded field by field and checked against `affinity_ttl_seconds`.
    #[serde(default)]
    #[schema(value_type = Option<KeepalivePolicyInput>)]
    keepalive: Option<Value>,
    /// A partial policy merged onto the pool's stored policy, which a create starts at the defaults: `enabled`
    /// `false`, `inject_when_absent` `false`, and two rule sets, `plan` with `main` `1h` and `subagent` `1h`, and
    /// `metered` with `main` `5m` and `subagent` `5m`. Each rule set merges one field at a time.
    #[serde(default)]
    #[schema(value_type = Option<CacheTtlPolicyInput>)]
    cache_ttl: Option<Value>,
    /// `true` or `false`, default `false`.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = bool)]
    cache_diagnostics: Option<Option<bool>>,
    /// `true` or `false`, default `true`.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = bool)]
    auto_mode_server: Option<Option<Value>>,
    /// At most `stats.body_capture_max_hours`.
    #[serde(default)]
    body_capture_hours: Option<u32>,
    /// A member list replaces the whole membership, so an absent budget clears it.
    #[serde(default)]
    members: Option<Vec<MemberInput>>,
    /// Default `true`.
    #[serde(default)]
    prefer_subscription: Option<bool>,
    /// Default `true`. Off, no shedding round runs. The spent move keeps `migrate_when_spent`.
    #[serde(default)]
    load_shedding: Option<bool>,
    /// `0.1` to `1.0`, default `0.5`.
    #[serde(default)]
    shed_cap: Option<f64>,
    /// `600` to `7200`, default `1800`.
    #[serde(default)]
    burn_average_seconds: Option<u64>,
    /// `300` to `7200`, default `900`.
    #[serde(default)]
    shed_dwell_seconds: Option<u64>,
    /// Default `true`.
    #[serde(default)]
    migrate_when_spent: Option<bool>,
    /// `always`, `never`, or `when_needed`, default `when_needed`.
    #[serde(default)]
    #[schema(example = "always")]
    warm_sessions: Option<String>,
    /// Default `false`. On, a request of a held class waits before it is forwarded for the pin's cycle times the
    /// account's slow-down, so the five-hour window reaches its reset under the ceiling. Auxiliary requests,
    /// compactions, the fast lane, probes and token counts are never held.
    #[serde(default)]
    slow_down: Option<bool>,
    /// `60` to `18000`, default `9000`. The time the slow-down may add to a session before a shedding round moves
    /// sessions off the account.
    #[serde(default)]
    slow_down_stretch_seconds: Option<u32>,
    /// `1` to `300`, default `120`. The longest one request waits for the slow-down.
    #[serde(default)]
    slow_down_hold_seconds: Option<u32>,
    /// `0` to `300`, default `60`.
    #[serde(default)]
    retry_in_place_seconds: Option<u64>,
    /// At least `1`, default `16`.
    #[serde(default)]
    in_flight_cap: Option<u32>,
    /// `0` to `300`, default `30`.
    #[serde(default)]
    overage_move_grace_minutes: Option<u32>,
    /// `0` to `10080`, a week, default `30`.
    #[serde(default)]
    wind_down_minutes: Option<u32>,
    /// `always`, `never`, or `when_cold`, default `always`. When a session on an API-key member returns to a
    /// subscription member.
    #[serde(default)]
    #[schema(example = "always")]
    return_to_plan: Option<String>,
    /// `0` to `86400`, default `900`. Under `always`, how long a warm session stays on the API-key member before it
    /// returns.
    #[serde(default)]
    return_to_plan_dwell_seconds: Option<u32>,
    /// `always` or `when_cached`, default `always`.
    #[serde(default)]
    #[schema(example = "always")]
    affinity_mode: Option<String>,
    /// `by_cohort`, `by_strategy`, or `with_main`, default `by_strategy`.
    #[serde(default)]
    #[schema(example = "by_cohort")]
    subagent_placement: Option<String>,
    /// `by_strategy` or `by_warm_prefix`, default `by_strategy`.
    #[serde(default)]
    #[schema(example = "by_strategy")]
    session_placement: Option<String>,
    /// Default `20000`.
    #[serde(default)]
    min_shared_prefix_tokens: Option<u64>,
    /// The pool's guardrail ids in the order its keys run them, replacing the pool's list whole, on an edit alone: a
    /// new pool starts with none, so a create refuses the field.
    #[serde(default)]
    guardrails: Option<Vec<String>>,
    /// Optional, the `revision` the row was read at, which the edit reads. A create accepts it and ignores it, since a
    /// create has no row to fence on. The write lands only while the pool still carries the `updated_at` the route
    /// read, or the `revision` the body sent here, so an edit that raced another write to the pool, a `PUT` of its
    /// members included, is refused rather than landing over it. A pool deleted and created again under the same id
    /// never answers a `revision` its predecessor did, so an edit read before the deletion answers `409` and writes
    /// nothing.
    // The raw value, kept apart from an absent field, so a present null, a string or a fraction is refused with
    // its own sentence rather than read as "not sent" or refused with the body's. A create reads nothing of it,
    // since there is no row to fence on.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = u64)]
    expected_revision: Option<Value>,
}

/// The keepalive policy's four fields, each optional.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeepalivePolicyInput {
    #[schema(nullable = false)]
    pub enabled: Option<bool>,
    /// The idle window of the main conversation, `0` to `31536000`, 0 for the recipe's break-even.
    #[schema(nullable = false)]
    pub max_idle_seconds: Option<u64>,
    /// `60` to `604800`, or null for the recipe's break-even.
    pub max_parked_seconds: Option<u64>,
    /// `60` to `604800`, or null for the recipe's break-even.
    pub scheduled_horizon_seconds: Option<u64>,
}

/// The cache TTL policy's two flags and two rule sets, each optional.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheTtlPolicyInput {
    #[schema(nullable = false)]
    pub enabled: Option<bool>,
    #[schema(nullable = false)]
    pub inject_when_absent: Option<bool>,
    pub metered: Option<TtlRulesInput>,
    pub plan: Option<TtlRulesInput>,
}

/// One rule set's overrides.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TtlRulesInput {
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub main: Option<String>,
    /// On canonical ids. An empty model id or a model listed twice is refused.
    pub models: Option<Vec<ModelTtlInput>>,
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub subagent: Option<String>,
}

// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelTtlInput {
    pub model: String,
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub main: String,
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub subagent: String,
}

/// A PATCH's policy names only the fields the operator touched, and the struct fills the rest with defaults on parse,
/// so overlaying it on the row's stored policy first keeps every untouched field at its current value. A `nested`
/// field merges one level deeper: a PATCH naming `{"metered": {"main": "5m"}}` overlays that field alone and leaves the
/// set's other three where they stood rather than falling back to the set's own defaults. A non-object `incoming` is
/// passed through unchanged so `parse`'s own shape check rejects it.
fn overlay(stored: &impl serde::Serialize, incoming: &Value, nested: &[&str]) -> Value {
    let Value::Object(patch) = incoming else {
        return incoming.clone();
    };
    let mut merged = serde_json::to_value(stored).unwrap_or_else(|_| json!({}));
    if let Value::Object(base) = &mut merged {
        for (key, value) in patch {
            if let (true, Value::Object(set_patch)) = (nested.contains(&key.as_str()), value) {
                if let Some(Value::Object(set_base)) = base.get_mut(key) {
                    set_base.extend(set_patch.clone());
                    continue;
                }
            }
            base.insert(key.clone(), value.clone());
        }
    }
    merged
}

/// Applies `input` over `row`, refusing every value the spec's pool table rules out.
// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
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
        row.strategy = Strategy::parse(strategy).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "strategy must be pace, priority, or round_robin",
            )
        })?;
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
        row.overage_policy = OveragePolicy::parse(policy).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "overage_policy must be prefer_plan, allow, or never",
            )
        })?;
    }
    if let Some(ceiling) = input.window_ceiling {
        if !(0.5..=1.0).contains(&ceiling) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "window_ceiling is between 0.5 and 1.0",
            ));
        }
        row.window_ceiling = ceiling;
    }
    if let Some(list) = &input.model_rules {
        row.model_rules = parse_rules(list)?;
    }
    if let Some(keepalive) = &input.keepalive {
        let stored = KeepalivePolicy::from_json(&row.keepalive_json);
        let merged = overlay(&stored, keepalive, &[]);
        let policy = KeepalivePolicy::parse(&merged)
            .map_err(|error| failure(StatusCode::BAD_REQUEST, &error.to_string()))?;
        // The stored text stays where it reads as the same policy, so a save naming the values a pool holds writes no
        // change row whatever spelling the text was stored in.
        if policy != stored {
            row.keepalive_json = serde_json::to_string(&policy).unwrap_or_else(|_| "{}".into());
        }
    }
    if let Some(cache_ttl) = &input.cache_ttl {
        let mut merged = overlay(
            &CacheTtlPolicy::from_json(&row.cache_ttl_json),
            cache_ttl,
            &["metered", "plan"],
        );
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
        let policy = CacheTtlPolicy::parse(&merged)
            .map_err(|error| failure(StatusCode::BAD_REQUEST, &error.to_string()))?;
        row.cache_ttl_json = serde_json::to_string(&policy).unwrap_or_else(|_| "{}".into());
    }
    if let Some(switch) = input.cache_diagnostics {
        // A null names neither state of the switch, so it is refused rather than read as one of them.
        row.cache_diagnostics = switch.ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "cache_diagnostics must be true or false",
            )
        })?;
    }
    if let Some(value) = &input.auto_mode_server {
        row.auto_mode_server = value.as_ref().and_then(Value::as_bool).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "auto_mode_server must be true or false",
            )
        })?;
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
    if let Some(on) = input.load_shedding {
        row.shedding.load_shedding = on;
    }
    if let Some(cap) = input.shed_cap {
        if !(0.1..=1.0).contains(&cap) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "shed_cap is between 0.1 and 1.0",
            ));
        }
        row.shedding.shed_cap = cap;
    }
    if let Some(seconds) = input.burn_average_seconds {
        if !(600..=7_200).contains(&seconds) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "burn_average_seconds is between 600 and 7200",
            ));
        }
        row.shedding.burn_average_seconds = seconds;
    }
    if let Some(seconds) = input.shed_dwell_seconds {
        if !(300..=7_200).contains(&seconds) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "shed_dwell_seconds is between 300 and 7200",
            ));
        }
        row.shedding.shed_dwell_seconds = seconds;
    }
    if let Some(spent) = input.migrate_when_spent {
        row.shedding.migrate_when_spent = spent;
    }
    if let Some(mode) = &input.warm_sessions {
        row.shedding.warm_sessions = WarmSessions::parse(mode).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "warm_sessions must be always, never, or when_needed",
            )
        })?;
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
    if let Some(on) = input.slow_down {
        row.slow_down.slow_down = on;
    }
    if let Some(seconds) = input.slow_down_stretch_seconds {
        if !(60..=18_000).contains(&seconds) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "slow_down_stretch_seconds is between 60 and 18000",
            ));
        }
        row.slow_down.slow_down_stretch_seconds = seconds;
    }
    if let Some(seconds) = input.slow_down_hold_seconds {
        if !(1..=300).contains(&seconds) {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "slow_down_hold_seconds is between 1 and 300",
            ));
        }
        row.slow_down.slow_down_hold_seconds = seconds;
    }
    if let Some(mode) = &input.return_to_plan {
        row.failover.return_to_plan = ReturnToPlan::parse(mode).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "return_to_plan must be always, when_cold, or never",
            )
        })?;
    }
    if let Some(seconds) = input.return_to_plan_dwell_seconds {
        if seconds > 86_400 {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "return_to_plan_dwell_seconds is between 0 and 86400",
            ));
        }
        row.failover.return_to_plan_dwell_seconds = seconds;
    }
    if let Some(mode) = &input.affinity_mode {
        row.affinity.mode = AffinityMode::parse(mode).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "affinity_mode must be always or when_cached",
            )
        })?;
    }
    if let Some(placement) = &input.subagent_placement {
        row.affinity.subagent_placement = SubagentPlacement::parse(placement).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "subagent_placement must be by_cohort, by_strategy, or with_main",
            )
        })?;
    }
    if let Some(placement) = &input.session_placement {
        row.affinity.session_placement = SessionPlacement::parse(placement).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "session_placement must be by_strategy or by_warm_prefix",
            )
        })?;
    }
    if let Some(tokens) = input.min_shared_prefix_tokens {
        row.affinity.min_shared_prefix_tokens = tokens;
    }
    // Checked over the whole row rather than over the edited field alone, so lowering affinity under an
    // enabled policy is refused exactly as enabling under a short affinity is.
    KeepalivePolicy::from_json(&row.keepalive_json)
        .validate(row.affinity_ttl_seconds)
        .map_err(|error| failure(StatusCode::BAD_REQUEST, &error.to_string()))?;
    Ok(())
}

/// List the pools.
#[utoipa::path(
    get,
    path = "/admin/pools",
    operation_id = "pools_list",
    tag = "Pools",
    responses(
        (status = 200, description = "The pools.", body = PoolList),
        (status = 500, response = StorageFailure),
    ),
)]
pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    let context = match pool_context(&state, None).await {
        Ok(context) => context,
        Err(error) => return store_failure(error),
    };
    let pools = context
        .pools
        .iter()
        .map(|row| pool_json_from(&context, row))
        .collect();
    Json(PoolList { pools }).into_response()
}

/// Create a pool.
#[utoipa::path(
    post,
    path = "/admin/pools",
    operation_id = "pools_create",
    tag = "Pools",
    request_body(content = PoolInput, content_type = "application/json"),
    responses(
        (status = 201, description = "The pool, the same shape as a row of `GET /admin/pools`.", body = Pool),
        (status = 400, description = "- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `id and label are required`\n- `id must match [a-z0-9][a-z0-9-]{0,63}`\n- `strategy must be pace, priority, or round_robin`\n- `affinity_ttl_seconds is 0 or at least 60`\n- `overage_policy must be prefer_plan, allow, or never`\n- `model_rules holds at most 256 entries`\n- `model_rules[{i}]: {reason}`: the entry's index and the grammar's own text\n- `keepalive is not a policy object: {detail}`: the keepalive policy's own sentence for a bad shape, or for a field the policy does not hold (`keepalive is not a policy object: unknown field ...`)\n- `keepalive.{field} must be between {min} and {max}, not {value}`: an out-of-range field (`keepalive.max_parked_seconds must be between 60 and 604800, not 59`)\n- `cache_ttl is not a policy object: {detail}`: the cache TTL policy's own sentence for a bad shape or an unknown field\n- `{set}[].model must be a model id`: an empty model id, `{set}` naming the rule set's list (`cache_ttl.plan.models` or `cache_ttl.metered.models`)\n- `{set} lists {model} twice`: a model listed twice\n- `cache_diagnostics must be true or false`\n- `auto_mode_server must be true or false`\n- `body_capture_hours exceeds stats.body_capture_max_hours`\n- `keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed`\n- `burn_average_seconds is between 600 and 7200`\n- `shed_cap is between 0.1 and 1.0`\n- `shed_dwell_seconds is between 300 and 7200`\n- `window_ceiling is between 0.5 and 1.0`\n- `warm_sessions must be always, never, or when_needed`\n- `retry_in_place_seconds is 0 to 300`\n- `in_flight_cap is at least 1`\n- `overage_move_grace_minutes is 0 to 300`\n- `wind_down_minutes is at most 10080 (one week)`\n- `return_to_plan must be always, when_cold, or never`\n- `return_to_plan_dwell_seconds is between 0 and 86400`\n- `slow_down_hold_seconds is between 1 and 300`\n- `slow_down_stretch_seconds is between 60 and 18000`\n- `affinity_mode must be always or when_cached`\n- `subagent_placement must be by_cohort, by_strategy, or with_main`\n- `session_placement must be by_strategy or by_warm_prefix`\n- `budget_daily_usd is above 0`\n- `budget_monthly_usd is above 0`\n- `guardrails applies on edit`: a new pool starts with no guardrail", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "`account {id} not found`, with nothing written", body = ErrorBody, example = json!({"error": "account {id} not found"})),
        (status = 409, description = "- `a pool with this id exists`\n- `account {id} is a member of pool {pool}`, with nothing written\n- `members name account {id} twice`, with nothing written", body = ErrorBody, example = json!({"error": "a pool with this id exists"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn create(State(state): State<Arc<AdminState>>, body: Bytes) -> Response {
    let input: PoolInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
    };
    // A new pool starts with no guardrail, so the create's transaction writes the pool and its members alone.
    if input.guardrails.is_some() {
        return failure(StatusCode::BAD_REQUEST, "guardrails applies on edit");
    }
    let (Some(id), Some(_)) = (&input.id, &input.label) else {
        return failure(StatusCode::BAD_REQUEST, "id and label are required");
    };
    if let Err(refusal) = check_slug(id) {
        return refusal;
    }
    if let Ok(Some(_)) = state.store.pool(id) {
        return failure(StatusCode::CONFLICT, DUPLICATE_ID);
    }
    let mut row = PoolRow {
        // Stored whole, so the create's change row names each member of the policy the pool starts with.
        keepalive_json: serde_json::to_string(&KeepalivePolicy::default())
            .unwrap_or_else(|_| "{}".into()),
        ..PoolRow::new(id)
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    let writes = match input.members.as_deref().map(member_writes).transpose() {
        Ok(writes) => writes,
        Err(refusal) => return refusal,
    };
    match state
        .store
        .insert_pool_with_members(&row, writes.as_deref(), ACTOR)
    {
        // The store takes the incarnation inside the insert, so the answer serializes the row it wrote.
        Ok(written) => row = written,
        // The check above reads outside the insert, so a create that lost the race for the id arrives here.
        Err(StoreError::IdTaken(_)) => return failure(StatusCode::CONFLICT, DUPLICATE_ID),
        // The row and its members are one transaction, so a refused member leaves no pool to name.
        Err(error) => return membership_failure(error),
    }
    // The row exists from here on, so every answer, a refusal included, names it to the mutation log.
    let mut response = publish(&state, &row, StatusCode::CREATED).await;
    response.extensions_mut().insert(CreatedId(row.id));
    response
}

/// The refusals a membership write shares: a member account that does not exist, or one another pool holds.
fn membership_failure(error: StoreError) -> Response {
    match error {
        StoreError::AccountNotFound(id) => {
            failure(StatusCode::NOT_FOUND, &format!("account {id} not found"))
        }
        StoreError::AccountInPool {
            account_id,
            pool_id,
        } => failure(
            StatusCode::CONFLICT,
            &format!("account {account_id} is a member of pool {pool_id}"),
        ),
        error => store_failure(error),
    }
}

async fn publish(state: &AdminState, row: &PoolRow, status: StatusCode) -> Response {
    if let Err(refusal) = reload_or_refuse(state).await {
        return refusal;
    }
    match pool_context(state, Some(&row.id)).await {
        Ok(context) => (status, Json(pool_json_from(&context, row))).into_response(),
        Err(e) => store_failure(e),
    }
}

/// Edit a pool.
#[utoipa::path(
    patch,
    path = "/admin/pools/{id}",
    operation_id = "pool_patch",
    tag = "Pools",
    params(("id" = String, Path, description = "The pool's id")),
    request_body(content = PoolInput, content_type = "application/json"),
    responses(
        (status = 200, description = "The pool, the same shape as a row of `GET /admin/pools`.", body = Pool),
        (status = 400, description = "- `body must be a JSON object`, also the answer to an unknown field in the body, or a value past its field's type\n- `expected_revision is the revision the row was read at`: a `null`, or a value that is not a non-negative integer\n- `strategy must be pace, priority, or round_robin`\n- `affinity_ttl_seconds is 0 or at least 60`\n- `overage_policy must be prefer_plan, allow, or never`\n- `model_rules holds at most 256 entries`\n- `model_rules[{i}]: {reason}`: the entry's index and the grammar's own text\n- `keepalive is not a policy object: {detail}`: the keepalive policy's own sentence for a bad shape, or for a field the policy does not hold (`keepalive is not a policy object: unknown field ...`)\n- `keepalive.{field} must be between {min} and {max}, not {value}`: an out-of-range field (`keepalive.max_parked_seconds must be between 60 and 604800, not 59`)\n- `cache_ttl is not a policy object: {detail}`: the cache TTL policy's own sentence for a bad shape or an unknown field\n- `{set}[].model must be a model id`: an empty model id, `{set}` naming the rule set's list (`cache_ttl.plan.models` or `cache_ttl.metered.models`)\n- `{set} lists {model} twice`: a model listed twice\n- `cache_diagnostics must be true or false`\n- `auto_mode_server must be true or false`\n- `body_capture_hours exceeds stats.body_capture_max_hours`\n- `keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed`\n- `burn_average_seconds is between 600 and 7200`\n- `shed_cap is between 0.1 and 1.0`\n- `shed_dwell_seconds is between 300 and 7200`\n- `window_ceiling is between 0.5 and 1.0`\n- `warm_sessions must be always, never, or when_needed`\n- `retry_in_place_seconds is 0 to 300`\n- `in_flight_cap is at least 1`\n- `overage_move_grace_minutes is 0 to 300`\n- `wind_down_minutes is at most 10080 (one week)`\n- `return_to_plan must be always, when_cold, or never`\n- `return_to_plan_dwell_seconds is between 0 and 86400`\n- `slow_down_hold_seconds is between 1 and 300`\n- `slow_down_stretch_seconds is between 60 and 18000`\n- `affinity_mode must be always or when_cached`\n- `subagent_placement must be by_cohort, by_strategy, or with_main`\n- `session_placement must be by_strategy or by_warm_prefix`\n- `budget_daily_usd is above 0`\n- `budget_monthly_usd is above 0`\n- `guardrails lists {id} twice`\n- `key {id} would hold more than 64 guardrails`, with nothing written\n- `pool {id} would hold more than 64 guardrails`, with nothing written", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "- `pool not found`\n- `account {id} not found`, with nothing written\n- `guardrail {id} not found`, with nothing written", body = ErrorBody, example = json!({"error": "pool not found"})),
        // The header goes ahead of `example`: utoipa-gen 6 takes the comma after an example as its own, so an
        // attribute after it fails to parse.
        (status = 409, description = "- `account {id} is a member of pool {pool}`, with nothing written\n- `members name account {id} twice`, with nothing written\n- `the pool changed while it was being edited, so reload and try again`, which carries the `stale_edit` code",
            headers(("x-phoebis-error-code" = String, description = "`stale_edit`: the row changed since it was read")),
            body = ErrorBody, example = json!({"error": "account {id} is a member of pool {pool}"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let input: PoolInput = match parse_object(&body) {
        Ok(input) => input,
        Err(response) => return response,
    };
    let mut row = match pool(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let read_at = match fence_token(
        input.expected_revision.as_ref(),
        row.incarnation,
        row.updated_at,
        STALE_EDIT,
    ) {
        Ok(stamp) => stamp,
        Err(refusal) => return refusal,
    };
    if let Err(refusal) = apply(&state, &mut row, &input) {
        return refusal;
    }
    let writes = match input.members.as_deref().map(member_writes).transpose() {
        Ok(writes) => writes,
        Err(refusal) => return refusal,
    };
    if let Some(guardrails) = &input.guardrails {
        if let Err(refusal) = distinct_ids("guardrails", guardrails) {
            return refusal;
        }
    }
    match state.store.update_pool_with_members_and_guardrails(
        &row,
        read_at,
        writes.as_deref(),
        input.guardrails.as_deref(),
        ACTOR,
    ) {
        Ok(at) => row.updated_at = at,
        Err(StoreError::Stale) => return stale_edit(STALE_EDIT),
        Err(StoreError::PoolNotFound(_)) => {
            return failure(StatusCode::NOT_FOUND, "pool not found")
        }
        Err(StoreError::GuardrailNotFound(id)) => {
            return failure(StatusCode::NOT_FOUND, &format!("guardrail {id} not found"))
        }
        Err(error @ (StoreError::PoolGuardrailBound(_) | StoreError::KeyGuardrailBound(_))) => {
            return attachment_refusal(error)
        }
        Err(error) => return membership_failure(error),
    }
    publish(&state, &row, StatusCode::OK).await
}

/// Delete a pool.
#[utoipa::path(
    delete,
    path = "/admin/pools/{id}",
    operation_id = "pool_delete",
    tag = "Pools",
    params(("id" = String, Path, description = "The pool's id")),
    responses(
        (status = 204, description = "The pool is deleted."),
        (status = 404, description = "`pool not found`", body = ErrorBody, example = json!({"error": "pool not found"})),
        (status = 409, description = "`pool still has keys`", body = ErrorBody, example = json!({"error": "pool still has keys"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn remove(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete_pool(&id, ACTOR) {
        Ok(()) => reload_or_no_content(&state).await,
        Err(StoreError::PoolInUse(_)) => failure(StatusCode::CONFLICT, "pool still has keys"),
        Err(StoreError::PoolNotFound(_)) => failure(StatusCode::NOT_FOUND, "pool not found"),
        Err(error) => store_failure(error),
    }
}

/// Replace the pool's members.
#[utoipa::path(
    put,
    path = "/admin/pools/{id}/members",
    operation_id = "pool_members_put",
    tag = "Pools",
    params(("id" = String, Path, description = "The pool's id")),
    request_body(content = MembersInput, description = "The same shape and validation as `members` on `POST /admin/pools`, replacing the whole membership list", content_type = "application/json"),
    responses(
        (status = 200, description = "The pool, the same shape as a row of `GET /admin/pools`.", body = Pool),
        (status = 400, description = "- `body must be a JSON object`, also the answer to an entry with an unknown field or a value past its field's type\n- `body must carry a members list`: an object whose `members` is absent or not an array\n- `budget_daily_usd is above 0`\n- `budget_monthly_usd is above 0`", body = ErrorBody, example = json!({"error": "body must be a JSON object"})),
        (status = 404, description = "- `pool not found`\n- `account {id} not found`", body = ErrorBody, example = json!({"error": "pool not found"})),
        (status = 409, description = "- `account {id} is a member of pool {pool}`, with nothing written\n- `members name account {id} twice`", body = ErrorBody, example = json!({"error": "account {id} is a member of pool {pool}"})),
        (status = 500, response = StorageFailure),
        (status = 503, response = RegistryUnavailable),
    ),
)]
pub(crate) async fn set_members(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    // The list is checked on the parsed object, so `members` stays required in the published input schema.
    let object: serde_json::Map<String, Value> = match parse_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    if !object.get("members").is_some_and(Value::is_array) {
        return failure(StatusCode::BAD_REQUEST, "body must carry a members list");
    }
    // Read from the bytes, where the derive refuses a repeated member that the object keeps at its last occurrence.
    let Ok(input) = serde_json::from_slice::<MembersInput>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "body must be a JSON object");
    };
    let mut row = match pool(&state, &id) {
        Ok(row) => row,
        Err(refusal) => return refusal,
    };
    let writes = match member_writes(&input.members) {
        Ok(writes) => writes,
        Err(refusal) => return refusal,
    };
    match state.store.set_pool_members(&id, &writes, ACTOR) {
        Ok(at) => row.updated_at = at,
        Err(error) => return membership_failure(error),
    }
    publish(&state, &row, StatusCode::OK).await
}

/// The pools, one row each.
// `Pool` serializes alone, so the list does too and the test-only `PoolListTwin` carries the round trip.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct PoolList {
    pub pools: Vec<Pool>,
}

/// A pool: the record every field of which the pool table names, its membership, its keys and its day.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct Pool {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    /// `pace`, `priority`, or `round_robin`.
    #[schema(example = "pace")]
    pub strategy: String,
    pub prefer_subscription: bool,
    pub affinity_ttl_seconds: u64,
    /// `allow`, `never`, or `prefer_plan`.
    #[schema(example = "allow")]
    pub overage_policy: String,
    /// `always` or `when_cached`.
    #[schema(example = "always")]
    pub affinity_mode: String,
    /// `by_cohort`, `by_strategy`, or `with_main`.
    #[schema(example = "by_cohort")]
    pub subagent_placement: String,
    /// `by_strategy` or `by_warm_prefix`.
    #[schema(example = "by_strategy")]
    pub session_placement: String,
    pub min_shared_prefix_tokens: u64,
    pub window_ceiling: f64,
    #[schema(value_type = Vec<crate::openapi::ModelRuleView>)]
    pub model_rules: Vec<phoebis_store::ModelRule>,
    /// The first of the shedding fields, with `shed_cap`, `burn_average_seconds`, `shed_dwell_seconds`,
    /// `migrate_when_spent` and `warm_sessions`.
    pub load_shedding: bool,
    pub shed_cap: f64,
    pub burn_average_seconds: u64,
    pub shed_dwell_seconds: u64,
    pub migrate_when_spent: bool,
    /// `always`, `never`, or `when_needed`.
    #[schema(example = "always")]
    pub warm_sessions: String,
    /// The first of the slow-down fields, with `slow_down_stretch_seconds` and `slow_down_hold_seconds`.
    pub slow_down: bool,
    pub slow_down_stretch_seconds: u32,
    pub slow_down_hold_seconds: u32,
    /// The first of the failover fields, with `in_flight_cap`, `overage_move_grace_minutes`, `wind_down_minutes`,
    /// `return_to_plan` and `return_to_plan_dwell_seconds`.
    pub retry_in_place_seconds: u64,
    pub in_flight_cap: u32,
    pub overage_move_grace_minutes: u32,
    pub wind_down_minutes: u32,
    /// `always`, `never`, or `when_cold`.
    #[schema(example = "always")]
    pub return_to_plan: String,
    pub return_to_plan_dwell_seconds: u32,
    /// Counts every move by reason over the last hour, scoped to the pool's own accounts, while the console's pool
    /// tile sums the five rule reasons alone (`drain`, `overage`, `return_to_plan`, `shed`, `spent`) and lists
    /// the failover reasons in its title.
    pub moves_per_hour: BTreeMap<String, f64>,
    /// The policy's four fields, `enabled`, `max_idle_seconds`, `max_parked_seconds` and `scheduled_horizon_seconds`,
    /// a ceiling left at the recipe's break-even answered as null.
    #[schema(value_type = crate::openapi::KeepalivePolicyView)]
    pub keepalive: KeepalivePolicy,
    /// Null for a pool with no member, else `break_even_seconds`, `horizon_seconds`, `idle_seconds`, `idle_share` and
    /// `parked_seconds`, each a `{min, max}` over the members' fundings, the main conversation's TTLs and the pool's
    /// models. The documentation site's keepalive page describes them under its four fields.
    #[schema(value_type = Option<crate::openapi::ResolvedCeilingsView>)]
    pub keepalive_resolved: Option<phoebis_keepalive::ResolvedCeilings>,
    /// The full cache TTL policy: `enabled`, `inject_when_absent`, and two rule sets, `plan` and `metered`, each with
    /// `main`, `subagent`, and `models`.
    #[schema(value_type = crate::openapi::CacheTtlPolicyView)]
    pub cache_ttl: CacheTtlPolicy,
    /// `true` or `false`.
    pub cache_diagnostics: bool,
    pub auto_mode_server: bool,
    pub body_capture_hours: u32,
    pub created_at: i64,
    pub updated_at: i64,
    /// The row's edit token, which `PATCH /admin/pools/{id}` takes back as `expected_revision`.
    pub revision: u64,
    pub members: Vec<PoolMemberView>,
    /// Filtered to this pool.
    pub keys: Vec<PoolKey>,
    /// `disabled`, `exhausted`, `degraded`, or `active`.
    #[schema(example = "disabled")]
    pub status: String,
    pub requests_24h: u64,
    pub cost_24h: f64,
    /// The dollars of the day's billed rows alone (fast mode, overage, API key accounts and usage-based seats), a part
    /// of `cost_24h`.
    pub billed_24h: f64,
    /// The day's plan-funded rows as the Pro-week figure, whatever tier served them, null where a row priced nothing
    /// and 0 with no plan-funded row.
    pub equivalent_24h: Option<f64>,
    /// The pins under the pool's keys.
    pub active_pins: usize,
    /// 25 hourly buckets from 24 hours before the current hour's start, the current partial hour last, whose
    /// `requests` sum to `requests_24h`.
    #[schema(value_type = Vec<crate::openapi::TrafficBucketView>)]
    pub traffic_24h: Vec<phoebis_ledger::TrafficBucket>,
    /// The pool's guardrails in position order, which every key of the pool runs ahead of its own.
    pub guardrails: Vec<GuardrailRef>,
    /// The static checks that hold on the pool's settings, sorted by `code`, empty when none does.
    pub warnings: Vec<PoolWarning>,
}

/// One static check that holds on the pool's settings. A warning blocks nothing.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolWarning {
    /// `affinity_under_cache_ttl`, `cap_under_cohort`, `cap_under_quarter`, `ceiling_low`, `ceiling_without_margin`,
    /// `hold_over_window`, `return_at_once`, `shedding_off`, `spent_moves_off`, or `warm_never`.
    #[schema(example = "ceiling_low")]
    pub code: String,
    /// The pool fields the check reads.
    pub fields: Vec<String>,
    /// The check's one sentence.
    pub text: String,
}

/// The checks of `row` that hold, sorted by code. `cohort` is the most agents of one session among the pins under the
/// pool's keys, from the router's pins at read time.
fn warnings_of(row: &PoolRow, cache_ttl: &CacheTtlPolicy, cohort: usize) -> Vec<PoolWarning> {
    let warning = |code: &str, fields: &[&str], text: String| PoolWarning {
        code: code.into(),
        fields: fields.iter().map(|f| f.to_string()).collect(),
        text,
    };
    let mut warnings = Vec::new();
    let longest_ttl = [
        cache_ttl.plan.main,
        cache_ttl.plan.subagent,
        cache_ttl.metered.main,
        cache_ttl.metered.subagent,
    ]
    .into_iter()
    .map(|ttl| match ttl {
        CacheTtl::FiveMinutes => 300,
        CacheTtl::OneHour => 3_600,
    })
    .max()
    .unwrap_or(0);
    // The relay injects only under a policy that is on, so an injection switch on a policy that is off injects nothing.
    if cache_ttl.enabled && cache_ttl.inject_when_absent && row.affinity_ttl_seconds < longest_ttl {
        warnings.push(warning(
            "affinity_under_cache_ttl",
            &["affinity_ttl_seconds", "cache_ttl"],
            "The session affinity ends before the cache TTL the pool injects, so a pin can expire while its cache is \
             warm and the next request can land cold elsewhere."
                .into(),
        ));
    }
    if (row.failover.in_flight_cap as usize) < cohort {
        warnings.push(warning(
            "cap_under_cohort",
            &["in_flight_cap"],
            format!(
                "The in-flight cap is under the largest live cohort of {cohort} agents, so subagents of one session \
                 wait for a slot they share."
            ),
        ));
    }
    if row.shedding.shed_cap < 0.25 {
        warnings.push(warning(
            "cap_under_quarter",
            &["shed_cap"],
            "A shed cap under 25 percent removes at most a quarter of the burn per round, so a window far over the \
             ceiling needs several rounds and dwells to come under it."
                .into(),
        ));
    }
    if row.window_ceiling < 0.8 {
        warnings.push(warning(
            "ceiling_low",
            &["window_ceiling"],
            "A ceiling under 80 percent leaves a fifth of every window unused and moves sessions that would have \
             finished the window warm."
                .into(),
        ));
    }
    if row.window_ceiling >= 1.0 {
        warnings.push(warning(
            "ceiling_without_margin",
            &["window_ceiling"],
            "A ceiling at 100 percent leaves no margin for a stale reading or a large turn, so a session can reach the \
             wall before the round or the gate reads it."
                .into(),
        ));
    }
    // With the slow-down on, a session over the ceiling is slowed toward the reset rather than left to spend its
    // window, so the sentence holds only while both switches are off.
    if !row.shedding.load_shedding && !row.slow_down.slow_down {
        // Both switches, since the check reads both: the console's dialog opens the group of the first.
        warnings.push(warning(
            "shedding_off",
            &["load_shedding", "slow_down"],
            "With load shedding off no round marks sessions on a member whose window would pass the ceiling, so a \
             session stays until its window is spent or its account refuses it."
                .into(),
        ));
    }
    if !row.shedding.migrate_when_spent {
        warnings.push(warning(
            "spent_moves_off",
            &["migrate_when_spent"],
            "With spent moves off a session stays on a spent account until the upstream refuses it, so the destination \
             gate never runs for that move."
                .into(),
        ));
    }
    if row.shedding.warm_sessions == WarmSessions::Never {
        warnings.push(warning(
            "warm_never",
            &["warm_sessions"],
            "With warm sessions never a round stops after the cold sessions, so the excess is covered only while cold \
             sessions carry it."
                .into(),
        ));
    }
    if row.slow_down.slow_down_hold_seconds > 150 {
        warnings.push(warning(
            "hold_over_window",
            &["slow_down_hold_seconds"],
            "A hold over 150 seconds leaves under 30 seconds of the client's first-byte window for the upstream's \
             head, so a slowed request can time out at the client and be sent again."
                .into(),
        ));
    }
    if row.failover.return_to_plan == ReturnToPlan::Always
        && row.failover.return_to_plan_dwell_seconds == 0
    {
        warnings.push(warning(
            "return_at_once",
            &["return_to_plan_dwell_seconds"],
            "A return dwell of zero returns every warm session on its first request at the metered member, so the \
             member serves one turn per session and the move's rewrite is paid each time."
                .into(),
        ));
    }
    warnings.sort_by(|a, b| a.code.cmp(&b.code));
    warnings
}

/// One member of a pool.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolMemberView {
    pub account_id: String,
    pub priority: u32,
    /// `budget`, `budget_unknown`, `cooldown`, `eligible`, `overage`, `spent`, `subscription_ended` (past the saved
    /// end of a canceled subscription), `unavailable` (deleted, paused or unready), or `unknown` (no projection: the
    /// router has not loaded the member, or does not answer).
    #[schema(example = "budget")]
    pub eligible: String,
    /// The account's `oauth` or `api_key`.
    #[schema(example = "oauth")]
    pub kind: Option<String>,
    /// Null for no budget.
    pub budget_daily_usd: Option<f64>,
    /// Null for no budget.
    pub budget_monthly_usd: Option<f64>,
    /// While a budget is reached, the end the member is held to, the latest end among the reached budgets. Null
    /// otherwise.
    pub budget_until: Option<i64>,
    /// One per budgeted period the ledger has reported, empty until it has.
    pub spends: Vec<SpendReport>,
    /// The pins under the pool's keys on that member's account, one per agent and model.
    pub active_pins: usize,
    /// The score Pace ranks the member by under this pool's settings, whatever its kind: the spare rate on the 7-day
    /// window for an OAuth member (the share it can spend an hour until the window resets, less its pins' burn), the
    /// room its worst live bucket leaves for an api_key member. A member's score reads no model, and the 7-day window
    /// is the one every model draws. Null while the router does not answer.
    pub pace_score: Option<f64>,
    /// The pins' burn on the member's 7-day window under this pool, in share per hour, null for an api_key member and
    /// a member with no reading of the window.
    pub burn: Option<f64>,
    /// The account's factor under the slow-down, the projection's `slow_down.factor`, null where `slow_down` is.
    pub factor: Option<f64>,
    /// The speed the holds realized, from 0 to 1, beside `factor`, null where `factor` is.
    pub speed: Option<f64>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpendReport {
    /// `day` or `month`.
    #[schema(example = "day")]
    pub period: String,
    pub spent_usd: f64,
    pub period_end: i64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolKey {
    pub id: String,
    pub label: String,
    pub prefix: String,
    pub enabled: bool,
}

/// A pool's traffic over the range, bucketed.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct PoolTraffic {
    pub pool_id: String,
    pub from: i64,
    pub to: i64,
    pub bucket_seconds: i64,
    #[schema(value_type = Vec<crate::openapi::TrafficBucketView>)]
    pub buckets: Vec<phoebis_ledger::TrafficBucket>,
}

/// One member row. Its own function, beside the pool's own struct literal, so one row's worth of fields reads
/// clearly on its own rather than nested inline inside the pool's larger one.
fn member_json(
    m: &PoolMember,
    accounts: &[phoebis_store::AccountRow],
    spends: &[phoebis_routing::MemberSpend],
    eligible: &str,
    now: i64,
    active_pins: usize,
    projection: Option<&phoebis_routing::AccountProjection>,
) -> PoolMemberView {
    let kind = accounts
        .iter()
        .find(|a| a.id == m.account_id)
        .map(|a| a.kind.as_str().to_string());
    let rows = crate::routes::spend_rows(spends, &m.pool_id, &m.account_id);
    let reports: Vec<SpendReport> = rows
        .iter()
        .filter(|s| m.budget(s.period).is_some())
        .map(|s| SpendReport {
            period: s.period.as_str().to_string(),
            spent_usd: s.spent_usd,
            period_end: s.period_end,
        })
        .collect();
    let slow_down = projection.and_then(|p| p.slow_down);
    PoolMemberView {
        account_id: m.account_id.clone(),
        priority: m.priority,
        eligible: eligible.to_string(),
        kind,
        budget_daily_usd: m.budget_daily_usd,
        budget_monthly_usd: m.budget_monthly_usd,
        budget_until: phoebis_routing::budget_deadline(m, &rows, now),
        spends: reports,
        active_pins,
        pace_score: projection.map(|p| p.pace_score),
        burn: projection
            .and_then(|p| p.windows.get(phoebis_routing::SEVEN_DAY))
            .map(|w| w.burn),
        factor: slow_down.map(|s| s.factor),
        speed: slow_down.map(|s| s.speed),
    }
}

/// What every pool row reads from the registry, the ledger and the router, fetched once per list call.
pub(crate) struct PoolContext {
    accounts: Vec<AccountRow>,
    ceilings: CeilingInputs,
    guardrails: AttachmentIndex,
    keys: Vec<ApiKeyRow>,
    members: Vec<PoolMember>,
    /// The last hour's moves by reason, by account.
    moves: BTreeMap<String, BTreeMap<String, u64>>,
    now: i64,
    pins: Vec<PinRow>,
    pools: Vec<PoolRow>,
    /// The router's projections of each pool's members under that pool's settings, by pool.
    projections: BTreeMap<String, Vec<phoebis_routing::AccountProjection>>,
    snapshot: Arc<RegistrySnapshot>,
    spends: Vec<MemberSpend>,
    /// Every pool's 25 buckets of the day, by pool.
    traffic: BTreeMap<String, Vec<phoebis_ledger::TrafficBucket>>,
}

/// One registry read for the pools, members, keys and accounts, one ledger read for the traffic of the pools it
/// answers and every account's moves, and one ask of the router for each of its tables, in the shape
/// of the account list's context. `pool` names the one pool a single-pool answer reads the traffic and the
/// projections of, and `None` reads them for every pool.
pub(crate) async fn pool_context(
    state: &AdminState,
    pool: Option<&str>,
) -> Result<PoolContext, StoreError> {
    let registry = state.store.load_registry()?;
    let guardrails = AttachmentIndex::from_load(&registry);
    let now = (state.clock)();
    let hour = phoebis_ledger::hour_of(now);
    let to = phoebis_ledger::hour_end(hour);
    let prices = state.pricing.revisions();
    let tables = state.tables.clone();
    let pool_ids: Vec<String> = match pool {
        Some(id) => vec![id.to_owned()],
        None => registry.pools.iter().map(|p| p.id.clone()).collect(),
    };
    // The day is 25 buckets, the current partial hour last, and a pool's day figures are their sums.
    let day_start = hour - 24 * 3_600;
    // A registry with no pool answers no row, so it reads nothing from the ledger.
    let ledger = async {
        if pool_ids.is_empty() {
            Default::default()
        } else {
            let ids = pool_ids.clone();
            state
                .ledger
                .db()
                .read(move |c| {
                    Ok((
                        phoebis_ledger::pools_traffic(
                            c, &ids, day_start, to, 3_600, &prices, &tables, now,
                        )?,
                        phoebis_ledger::pin_migrations_by_account_and_reason(
                            c,
                            now - 3_600_000,
                            now,
                        )?,
                    ))
                })
                .await
                .unwrap_or_default()
        }
    };
    let projections = async {
        let mut projections = BTreeMap::new();
        for id in &pool_ids {
            if let Ok(members) = state.router.projections(id).await {
                projections.insert(id.clone(), members);
            }
        }
        projections
    };
    // Each read is a snapshot of its own, so they wait together and their order carries no meaning.
    let ((traffic, moves), ceilings, pins, spends, projections) = tokio::join!(
        ledger,
        state.keepalive.ceiling_inputs(),
        state.router.pins(),
        state.router.member_spends(),
        projections,
    );
    Ok(PoolContext {
        accounts: registry.accounts,
        ceilings,
        guardrails,
        keys: registry.keys,
        members: registry.members,
        moves,
        now,
        pins: pins.unwrap_or_default(),
        pools: registry.pools,
        projections,
        snapshot: state.router.snapshot(),
        spends: spends.unwrap_or_default(),
        traffic,
    })
}

/// A member's standing as `PoolMemberView::eligible` answers it.
#[derive(Clone, Copy, PartialEq)]
enum Eligibility {
    Budget,
    BudgetUnknown,
    Cooldown,
    Eligible,
    Overage,
    Spent,
    SubscriptionEnded,
    Unavailable,
    Unknown,
}

impl Eligibility {
    fn as_str(self) -> &'static str {
        match self {
            Eligibility::Budget => "budget",
            Eligibility::BudgetUnknown => "budget_unknown",
            Eligibility::Cooldown => "cooldown",
            Eligibility::Eligible => "eligible",
            Eligibility::Overage => "overage",
            Eligibility::Spent => "spent",
            Eligibility::SubscriptionEnded => "subscription_ended",
            Eligibility::Unavailable => "unavailable",
            Eligibility::Unknown => "unknown",
        }
    }
}

/// The member word for the selector's obstacle on the member, as the router's projection of it answers.
fn member_word(projection: Option<&AccountProjection>) -> Eligibility {
    // The router answers no projection for a member it has not loaded (a registry write before its reload) or while
    // it does not answer, and routes that member nothing at that instant. A member with no usage reading still has a
    // projection, so it reads `eligible` or its obstacle's word. No account is attempted and the fast lane is off for
    // a projection, so the attempted and credits obstacles never arise, and neither takes a member out of the pool's
    // standard set.
    let Some(projection) = projection else {
        return Eligibility::Unknown;
    };
    match &projection.obstacle {
        None | Some(Obstacle::Attempted | Obstacle::Credits { .. }) => Eligibility::Eligible,
        Some(Obstacle::Budget { .. }) => Eligibility::Budget,
        Some(Obstacle::BudgetUnknown) => Eligibility::BudgetUnknown,
        Some(Obstacle::Cooldown { .. }) => Eligibility::Cooldown,
        Some(Obstacle::Ended) => Eligibility::SubscriptionEnded,
        Some(Obstacle::Deleted | Obstacle::Paused | Obstacle::Unready) => Eligibility::Unavailable,
        Some(Obstacle::Overage { .. }) => Eligibility::Overage,
        Some(Obstacle::Spent { .. }) => Eligibility::Spent,
    }
}

/// One pool row from the shared context, whose one read carries every pool's traffic and moves and whose ceiling
/// inputs price every pool alike, so a row takes no ledger read, no store lock and no ask of the router.
pub(crate) fn pool_json_from(context: &PoolContext, row: &PoolRow) -> Pool {
    let mut members: Vec<PoolMember> = context
        .members
        .iter()
        .filter(|m| m.pool_id == row.id)
        .cloned()
        .collect();
    members.sort_by(|a, b| (a.priority, &a.account_id).cmp(&(b.priority, &b.account_id)));
    let keys: Vec<PoolKey> = context
        .keys
        .iter()
        .filter(|k| k.pool_id == row.id)
        .map(|k| PoolKey {
            id: k.id.clone(),
            label: k.label.clone(),
            prefix: k.prefix.clone(),
            enabled: k.enabled,
        })
        .collect();
    let now = context.now;
    let traffic = context.traffic.get(&row.id).cloned().unwrap_or_default();
    let day = traffic
        .iter()
        .fold(phoebis_ledger::Totals::default(), |mut day, bucket| {
            day.add(&phoebis_ledger::Totals {
                cost_usd: bucket.cost_usd,
                requests: bucket.requests,
                tokens: bucket.tokens,
                billed_usd: bucket.billed_usd,
                equivalent: bucket.equivalent,
            });
            day
        });
    let mut moves: BTreeMap<String, u64> = BTreeMap::new();
    for member in &members {
        for (reason, count) in context.moves.get(&member.account_id).into_iter().flatten() {
            *moves.entry(reason.clone()).or_default() += count;
        }
    }
    let pins = &context.pins;
    let key_ids: Vec<String> = keys.iter().map(|k| k.id.clone()).collect();
    let mut pins_by_account: HashMap<&str, usize> = HashMap::new();
    let mut cohorts: HashMap<(&str, &str), HashSet<&str>> = HashMap::new();
    for pin in pins.iter().filter(|p| key_ids.contains(&p.key_id)) {
        *pins_by_account.entry(pin.account_id.as_str()).or_default() += 1;
        cohorts
            .entry((pin.key_id.as_str(), pin.session_id.as_str()))
            .or_default()
            .insert(pin.agent_id.as_str());
    }
    let active_pins: usize = pins_by_account.values().sum();
    let cohort = cohorts.values().map(HashSet::len).max().unwrap_or(0);
    let accounts = &context.accounts;
    let spends = &context.spends;
    let snapshot = &context.snapshot;
    let projections = context.projections.get(&row.id);
    let verdicts: Vec<(Option<&AccountProjection>, Eligibility)> = members
        .iter()
        .map(|m| {
            let projection = projections
                .and_then(|members| members.iter().find(|p| p.account_id == m.account_id));
            (projection, member_word(projection))
        })
        .collect();
    let ineligible = verdicts
        .iter()
        .filter(|(_, word)| *word != Eligibility::Eligible)
        .count();
    // A pool with no member has none to route to, so it reads exhausted as one whose every member is skipped.
    let status = if !row.enabled {
        "disabled"
    } else if ineligible == verdicts.len() {
        "exhausted"
    } else if ineligible > 0 {
        "degraded"
    } else {
        "active"
    };
    let keepalive = KeepalivePolicy::from_json(&row.keepalive_json);
    let cache_ttl = CacheTtlPolicy::from_json(&row.cache_ttl_json);
    let fundings = pool_fundings(
        accounts
            .iter()
            .filter(|a| members.iter().any(|m| m.account_id == a.id))
            .map(|a| (a.kind, snapshot.usage_based(&a.id))),
    );
    let keepalive_resolved =
        context
            .ceilings
            .resolve_pool(&keepalive, &cache_ttl, &row.model_rules, &fundings);
    let warnings = warnings_of(row, &cache_ttl, cohort);
    Pool {
        id: row.id.clone(),
        label: row.label.clone(),
        enabled: row.enabled,
        strategy: row.strategy.as_str().to_string(),
        prefer_subscription: row.prefer_subscription,
        affinity_ttl_seconds: row.affinity_ttl_seconds,
        overage_policy: row.overage_policy.as_str().to_string(),
        affinity_mode: row.affinity.mode.as_str().to_string(),
        subagent_placement: row.affinity.subagent_placement.as_str().to_string(),
        session_placement: row.affinity.session_placement.as_str().to_string(),
        min_shared_prefix_tokens: row.affinity.min_shared_prefix_tokens,
        window_ceiling: row.window_ceiling,
        model_rules: row.model_rules.clone(),
        load_shedding: row.shedding.load_shedding,
        shed_cap: row.shedding.shed_cap,
        burn_average_seconds: row.shedding.burn_average_seconds,
        shed_dwell_seconds: row.shedding.shed_dwell_seconds,
        migrate_when_spent: row.shedding.migrate_when_spent,
        warm_sessions: row.shedding.warm_sessions.as_str().to_string(),
        slow_down: row.slow_down.slow_down,
        slow_down_stretch_seconds: row.slow_down.slow_down_stretch_seconds,
        slow_down_hold_seconds: row.slow_down.slow_down_hold_seconds,
        retry_in_place_seconds: row.failover.retry_in_place_seconds,
        in_flight_cap: row.failover.in_flight_cap,
        overage_move_grace_minutes: row.failover.overage_move_grace_minutes,
        wind_down_minutes: row.failover.wind_down_minutes,
        return_to_plan: row.failover.return_to_plan.as_str().to_string(),
        return_to_plan_dwell_seconds: row.failover.return_to_plan_dwell_seconds,
        moves_per_hour: moves
            .iter()
            .map(|(reason, count)| (reason.clone(), *count as f64))
            .collect(),
        keepalive,
        keepalive_resolved,
        cache_ttl,
        cache_diagnostics: row.cache_diagnostics,
        auto_mode_server: row.auto_mode_server,
        body_capture_hours: row.body_capture_hours,
        created_at: row.created_at,
        updated_at: row.updated_at,
        revision: stamp_token(row.incarnation, row.updated_at),
        members: members
            .iter()
            .zip(&verdicts)
            .map(|(m, (projection, word))| {
                let active_pins = pins_by_account
                    .get(m.account_id.as_str())
                    .copied()
                    .unwrap_or(0);
                member_json(
                    m,
                    accounts,
                    spends,
                    word.as_str(),
                    now,
                    active_pins,
                    *projection,
                )
            })
            .collect(),
        keys,
        status: status.to_string(),
        requests_24h: day.requests,
        cost_24h: day.cost_usd,
        billed_24h: day.billed_usd,
        equivalent_24h: day.equivalent,
        active_pins,
        traffic_24h: traffic,
        guardrails: context
            .guardrails
            .refs_for(&AttachmentSubject::Pool(row.id.clone())),
        warnings,
    }
}

/// `GET /admin/pools/{id}/traffic`'s query.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct TrafficQuery {
    /// `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. Default `24h`.
    pub range: Option<String>,
}

/// Read the pool's traffic buckets.
#[utoipa::path(
    get,
    path = "/admin/pools/{id}/traffic",
    operation_id = "pool_traffic",
    tag = "Pools",
    params(("id" = String, Path, description = "The pool's id"), TrafficQuery),
    responses(
        (status = 200, description = "The buckets over the range.", body = PoolTraffic),
        (status = 400, description = concat!("For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\nFor a range the chart cannot carry, the ledger's own message: `the range needs {count} points, and the most a chart carries is 10000`.\n\n", query_refusal!()), body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 404, description = "`pool not found`", body = ErrorBody, example = json!({"error": "pool not found"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn traffic(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    AdminQuery(q): AdminQuery<TrafficQuery>,
) -> Response {
    if let Err(refusal) = pool(&state, &id) {
        return refusal;
    }
    let now = (state.clock)();
    let (from, to) = match phoebis_ledger::parse_range(q.range.as_deref(), now) {
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
        state.pricing.revisions(),
        state.tables.clone(),
        now,
    )
    .await
    {
        Ok(buckets) => Json(PoolTraffic {
            pool_id: id,
            from,
            to,
            bucket_seconds: bucket,
            buckets,
        })
        .into_response(),
        Err(e) => crate::analytics::ledger_failure(e),
    }
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::{round_trips, round_trips_each};
    use crate::openapi::{
        CacheTtlPolicyView, KeepalivePolicyView, ModelRuleView, ResolvedCeilingsView,
        TrafficBucketView,
    };
    use std::collections::BTreeMap;

    /// A pool with its five mirrored fields typed as the mirrors, so the whole row round trips.
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PoolTwin {
        id: String,
        label: String,
        enabled: bool,
        strategy: String,
        prefer_subscription: bool,
        affinity_ttl_seconds: u64,
        overage_policy: String,
        affinity_mode: String,
        subagent_placement: String,
        session_placement: String,
        min_shared_prefix_tokens: u64,
        window_ceiling: f64,
        model_rules: Vec<ModelRuleView>,
        load_shedding: bool,
        shed_cap: f64,
        burn_average_seconds: u64,
        shed_dwell_seconds: u64,
        migrate_when_spent: bool,
        warm_sessions: String,
        slow_down: bool,
        slow_down_stretch_seconds: u32,
        slow_down_hold_seconds: u32,
        retry_in_place_seconds: u64,
        in_flight_cap: u32,
        overage_move_grace_minutes: u32,
        wind_down_minutes: u32,
        return_to_plan: String,
        return_to_plan_dwell_seconds: u32,
        moves_per_hour: BTreeMap<String, f64>,
        keepalive: KeepalivePolicyView,
        keepalive_resolved: Option<ResolvedCeilingsView>,
        cache_ttl: CacheTtlPolicyView,
        cache_diagnostics: bool,
        auto_mode_server: bool,
        body_capture_hours: u32,
        created_at: i64,
        updated_at: i64,
        revision: u64,
        members: Vec<super::PoolMemberView>,
        keys: Vec<super::PoolKey>,
        status: String,
        requests_24h: u64,
        cost_24h: f64,
        billed_24h: f64,
        equivalent_24h: Option<f64>,
        active_pins: usize,
        traffic_24h: Vec<TrafficBucketView>,
        guardrails: Vec<crate::guardrails::GuardrailRef>,
        warnings: Vec<super::PoolWarning>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PoolListTwin {
        pools: Vec<PoolTwin>,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PoolTrafficTwin {
        pool_id: String,
        from: i64,
        to: i64,
        bucket_seconds: i64,
        buckets: Vec<TrafficBucketView>,
    }

    #[test]
    fn the_pool_fixtures_round_trip_through_their_declared_types_and_mirrors() {
        round_trips::<PoolListTwin>("pools");
        round_trips::<PoolTrafficTwin>("pool_traffic");
        round_trips_each::<KeepalivePolicyView>("pools", &["pools", "*", "keepalive"]);
        round_trips_each::<CacheTtlPolicyView>("pools", &["pools", "*", "cache_ttl"]);
        round_trips_each::<TrafficBucketView>("pools", &["pools", "*", "traffic_24h", "*"]);
    }
}
