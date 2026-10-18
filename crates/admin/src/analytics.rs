//! Rollup reads: the series bucketed to the range, the breakdown per group, and the account rollups.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    breakdown_paged, bucket_seconds, burn_groups_paged, held_series, hour_ceil, hour_of,
    hourly_window_samples, lead_over_pace, moves_by_reason_and_fit, names_one_account, parse_range,
    read_account_rollups, read_reconciliations, rewrite_series, series_paged, session_burn_series,
    AccountRollupRow, FundingFilter, HourlyWindowSample, LedgerError, LiveKeys, RollupFilter,
    SeriesFilter, SharePricing, GROUP_BYS,
};
use phoebis_routing::{term, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::{AccountKind, SubscriptionTerm};
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::openapi::{ErrorBody, LedgerFailure, RouterUnavailable};
use crate::routes::{failure, store_failure};
use crate::AdminState;

/// `session` beside the page's group-by dimensions: only the burn answers it, from the attempts, since the hourly
/// rollups carry no session.
const BURN_GROUP_BYS: &[&str] = &[
    "account",
    "client",
    "effort",
    "key",
    "kind",
    "model",
    "pool",
    "request_class",
    "session",
    "speed",
    "user_agent",
];

fn list(q: &HashMap<String, String>, name: &str) -> Vec<String> {
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

/// The same list, split on newlines instead of commas: a pattern of the wildcard grammar carries no newline, and the
/// request log's raw agents carry a comma (`claude-cli/2.1.269 (external, sdk-cli)`) that a pattern written against
/// them keeps.
fn agent_list(q: &HashMap<String, String>, name: &str) -> Vec<String> {
    q.get(name)
        .map(|v| {
            v.split('\n')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn flag(q: &HashMap<String, String>, name: &str) -> bool {
    q.get(name).is_some_and(|v| v == "true" || v == "1")
}

fn filter_of(q: &HashMap<String, String>) -> RollupFilter {
    RollupFilter {
        keys: list(q, "keys"),
        pools: list(q, "pools"),
        accounts: list(q, "accounts"),
        models: list(q, "models"),
        kinds: list(q, "kinds"),
        user_agents: agent_list(q, "user_agents"),
        user_agent_groups: agent_list(q, "user_agent_groups"),
        clients: agent_list(q, "clients"),
        include_probes: flag(q, "include_probes"),
        // Read by `selection` alone: `placements` builds its filter here and never takes the parameter.
        funding: None,
    }
}

/// The range in Unix milliseconds, the group-by, the filter, the live keys' generations the key dimension is read
/// against, and the registry's account kinds.
type Selection = (
    (i64, i64),
    String,
    RollupFilter,
    LiveKeys,
    HashMap<String, AccountKind>,
);

/// The registry's account kinds by id, which name a burn group's unit.
#[allow(clippy::result_large_err)]
fn kinds(state: &AdminState) -> Result<HashMap<String, AccountKind>, Response> {
    let accounts = state.store.accounts().map_err(store_failure)?;
    Ok(accounts.iter().map(|a| (a.id.clone(), a.kind)).collect())
}

/// Every analytics route starts here.
// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
async fn selection(state: &AdminState, q: &HashMap<String, String>) -> Result<Selection, Response> {
    let (from, to) = parse_range(q.get("range").map(String::as_str), (state.clock)())
        .map_err(|m| failure(StatusCode::BAD_REQUEST, &m))?;
    let group_by = q.get("group_by").cloned().unwrap_or_else(|| "all".into());
    if !GROUP_BYS.contains(&group_by.as_str()) {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "group_by is account, all, client, effort, key, kind, model, pool, request_class, speed, or user_agent",
        ));
    }
    let mut filter = filter_of(q);
    if !phoebis_ledger::patterns_within_bounds(&filter.user_agents)
        || !phoebis_ledger::patterns_within_bounds(&filter.user_agent_groups)
        || !phoebis_ledger::patterns_within_bounds(&filter.clients)
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            phoebis_ledger::PATTERNS_TOO_LONG,
        ));
    }
    filter.funding = match q.get("funding").map(String::as_str) {
        None => None,
        Some("billed") => Some(FundingFilter::Billed),
        Some("plan") => Some(FundingFilter::Plan),
        Some(_) => {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "funding is billed or plan",
            ))
        }
    };
    let kinds = kinds(state)?;
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    Ok(((from, to), group_by, filter, live, kinds))
}

/// The query of `GET /admin/analytics/accounts` and `GET /admin/analytics/rewrites`.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RangeQuery {
    /// `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. Default `24h`.
    pub range: Option<String>,
}

/// The query of `GET /admin/analytics/breakdown` and `GET /admin/analytics/series`.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct RollupQuery {
    /// `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. Default `24h`.
    pub range: Option<String>,
    /// `account`, `all`, `client`, `effort`, `key`, `kind`, `model`, `pool`, `request_class`, `speed`, or
    /// `user_agent`. Default `all`.
    pub group_by: Option<String>,
    /// A comma-separated list. A value names the key's id, or `<id>@<generation>` for an earlier holder's rows,
    /// and a bare id selects the current key's rows alone.
    pub keys: Option<String>,
    /// A comma-separated list.
    pub pools: Option<String>,
    /// A comma-separated list.
    pub accounts: Option<String>,
    /// A comma-separated list.
    pub models: Option<String>,
    /// A comma-separated list.
    pub kinds: Option<String>,
    /// Newline-separated patterns over the agent's first product token, the grammar the Analytics tag describes.
    pub user_agents: Option<String>,
    /// Newline-separated patterns, each naming a group under `group_by=user_agent` as the Analytics tag describes.
    pub user_agent_groups: Option<String>,
    /// Newline-separated patterns over the client class, the grammar the Analytics tag describes.
    pub clients: Option<String>,
    /// `true`/`1`.
    pub include_probes: Option<bool>,
    /// `billed` or `plan`, the two subsets the Analytics tag describes.
    pub funding: Option<String>,
}

/// `GET /admin/analytics/placements`'s query.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct PlacementsQuery {
    /// `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. Default `24h`.
    pub range: Option<String>,
    /// A comma-separated list.
    pub keys: Option<String>,
    /// A comma-separated list.
    pub pools: Option<String>,
    /// A comma-separated list.
    pub accounts: Option<String>,
    /// A comma-separated list.
    pub models: Option<String>,
    /// A comma-separated list.
    pub kinds: Option<String>,
    /// Newline-separated patterns.
    pub user_agents: Option<String>,
    /// Newline-separated patterns, held to the bound `/breakdown` holds them to (at most 32 patterns of at most 256
    /// characters, a `400` past it) and grouping nothing here.
    pub user_agent_groups: Option<String>,
    /// Newline-separated patterns.
    pub clients: Option<String>,
    /// `true`/`1`.
    pub include_probes: Option<bool>,
}

/// `GET /admin/analytics/burn`'s query.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct BurnQuery {
    /// `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. Default `24h`.
    pub range: Option<String>,
    /// `account`, `client`, `effort`, `key`, `kind`, `model`, `pool`, `request_class`, `session`, `speed`, or
    /// `user_agent`. Default `account`. The `session` grouping reads the twenty sessions with the most tokens over the
    /// range, since the hourly rollups carry no session dimension. The rest read the hourly rollups the way
    /// `breakdown` and `series` do.
    pub group_by: Option<String>,
    /// A comma-separated list. Naming exactly one account answers `unit` and the shares under any grouping.
    pub accounts: Option<String>,
    /// Newline-separated patterns, each naming a group under `group_by=user_agent` as the Analytics tag describes.
    pub user_agent_groups: Option<String>,
}

/// `GET /admin/analytics/reconciliations`' query.
// Declared for the document alone, since the handler reads the request by hand.
#[allow(dead_code)]
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ReconciliationsQuery {
    /// A comma-separated list. Absent answers every account's windows.
    pub accounts: Option<String>,
}

pub(crate) fn ledger_failure(error: phoebis_ledger::LedgerError) -> Response {
    tracing::warn!(%error, "an admin read failed in the ledger");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "ledger failure")
}

/// The series bucketed to the range. Under `group_by=key` a point's `groups` entries carry `key` in the same form
/// as the breakdown route's key row. Each entry carries the breakdown row's `cost_shares`, `cache_shares`,
/// `probe_shares`, `saving_shares`, `plan_requests` and `plan_output_tokens` under the same rules, summed over the
/// bucket's rows, and its `unpriced` and `probe_unpriced` read off the bucket's rows alone.
// `GroupRow` serializes alone, so the series does too and the test-only `SeriesTwin` carries the round trip.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct Series {
    pub from: i64,
    pub to: i64,
    pub bucket_seconds: i64,
    /// `account`, `all`, `client`, `effort`, `key`, `kind`, `model`, `pool`, `request_class`, `speed`, or
    /// `user_agent`.
    #[schema(example = "account")]
    pub group_by: String,
    #[schema(value_type = Vec<crate::openapi::SeriesPointView>)]
    pub points: Vec<phoebis_ledger::SeriesPoint>,
}

/// Read the series per group.
#[utoipa::path(
    get,
    path = "/admin/analytics/series",
    operation_id = "analytics_series",
    tag = "Analytics",
    params(RollupQuery),
    responses(
        (status = 200, description = "The points over the range.", body = Series),
        (status = 400, description = "For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\nFor a range the chart cannot carry, the ledger's own message: `pricing table: the range needs {count} points, and the most a chart carries is 10000`.\n\nFor the other parameters:\n- `funding is billed or plan`\n- `group_by is account, all, client, effort, key, kind, model, pool, request_class, speed, or user_agent`\n- `patterns are at most 256 characters, at most 32 of them`, for `user_agents`, `user_agent_groups` or `clients`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn series(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ((from, to), group_by, filter, live, _) = match selection(&state, &q).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let bucket = bucket_seconds(from, to);
    // The ledger answers the shares where each group holds one account's rows.
    let pricing = Some(SharePricing {
        tables: &state.tables,
    });
    let points = match series_paged(
        state.ledger.db(),
        &filter,
        &live,
        from,
        to,
        bucket,
        &group_by,
        pricing,
        &state.pricing.revisions(),
        &state.tables,
    )
    .await
    {
        Ok(p) => p,
        // The range's own refusal, raised before any read.
        Err(e @ LedgerError::Yaml(_)) => return failure(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e) => return ledger_failure(e),
    };
    Json(Series {
        from,
        to,
        bucket_seconds: bucket,
        group_by,
        points,
    })
    .into_response()
}

/// The breakdown per group.
// `GroupRow` serializes alone, so the breakdown does too and the test-only `BreakdownTwin` carries the round trip.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct Breakdown {
    pub from: i64,
    pub to: i64,
    /// `account`, `all`, `client`, `effort`, `key`, `kind`, `model`, `pool`, `request_class`, `speed`, or
    /// `user_agent`.
    #[schema(example = "account")]
    pub group_by: String,
    #[schema(value_type = Vec<crate::openapi::BreakdownRow>)]
    pub rows: Vec<phoebis_ledger::GroupRow>,
}

/// Read the breakdown per group.
#[utoipa::path(
    get,
    path = "/admin/analytics/breakdown",
    operation_id = "analytics_breakdown",
    tag = "Analytics",
    params(RollupQuery),
    responses(
        (status = 200, description = "The rows over the range.", body = Breakdown),
        (status = 400, description = "For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\nFor the other parameters:\n- `funding is billed or plan`\n- `group_by is account, all, client, effort, key, kind, model, pool, request_class, speed, or user_agent`\n- `patterns are at most 256 characters, at most 32 of them`, for `user_agents`, `user_agent_groups` or `clients`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn breakdown(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ((from, to), group_by, filter, live, _) = match selection(&state, &q).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    // The ledger answers the shares where each group holds one account's rows.
    let pricing = Some(SharePricing {
        tables: &state.tables,
    });
    let rows = match breakdown_paged(
        state.ledger.db(),
        &filter,
        &live,
        from,
        to,
        &group_by,
        pricing,
        &state.pricing.revisions(),
        &state.tables,
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    Json(Breakdown {
        from,
        to,
        group_by,
        rows,
    })
    .into_response()
}

/// One row per budgeted period of a pool member.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Budgets {
    pub members: Vec<BudgetMember>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetMember {
    pub pool_id: String,
    pub account_id: String,
    /// The member's account kind, `oauth` or `api_key`.
    #[schema(example = "oauth")]
    pub kind: Option<String>,
    pub budget_usd: f64,
    /// `day` or `month`.
    #[schema(example = "day")]
    pub budget_period: String,
    /// Null until a spend report has landed.
    pub spent_usd: Option<f64>,
    /// Null until a spend report has landed.
    pub period_end: Option<i64>,
}

/// Read the members' budgets.
#[utoipa::path(
    get,
    path = "/admin/analytics/budgets",
    operation_id = "analytics_budgets",
    tag = "Analytics",
    responses(
        (status = 200, description = "The budgeted periods.", body = Budgets),
        (status = 503, response = RouterUnavailable),
    ),
)]
pub(crate) async fn budgets(State(state): State<Arc<AdminState>>) -> Response {
    let snapshot = state.router.snapshot();
    let Ok(spends) = state.router.member_spends().await else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        );
    };
    // One row per budgeted period, so a member with both budgets reads twice, its day before its month.
    let mut members: Vec<(String, String, phoebis_store::BudgetPeriod, BudgetMember)> = snapshot
        .budgeted_members()
        .flat_map(|m| {
            let rows = crate::routes::spend_rows(&spends, &m.pool_id, &m.account_id);
            let kind = snapshot
                .account(&m.account_id)
                .map(|a| crate::routes::kind_name(a.kind));
            m.budgets()
                .map(|(period, budget)| {
                    let spend = rows.iter().find(|s| s.period == period);
                    let row = BudgetMember {
                        pool_id: m.pool_id.clone(),
                        account_id: m.account_id.clone(),
                        kind: kind.map(String::from),
                        budget_usd: budget,
                        budget_period: period.as_str().into(),
                        spent_usd: spend.map(|s| s.spent_usd),
                        period_end: spend.map(|s| s.period_end),
                    };
                    (m.pool_id.clone(), m.account_id.clone(), period, row)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let month =
        |period: &phoebis_store::BudgetPeriod| *period == phoebis_store::BudgetPeriod::Month;
    members.sort_by(|a, b| (&a.0, &a.1, month(&a.2)).cmp(&(&b.0, &b.1, month(&b.2))));
    let members = members.into_iter().map(|(.., row)| row).collect();
    Json(Budgets { members }).into_response()
}

/// One row per rung reached.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placements {
    pub from: i64,
    pub to: i64,
    pub rows: Vec<PlacementRow>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlacementRow {
    /// `content`, `static`, or `with_main`.
    #[schema(example = "content")]
    pub rung: String,
    pub placements: u64,
    pub hits: u64,
    /// `hits` over `placements`, null at zero.
    pub hit_share: Option<f64>,
    pub cache_read_tokens: u64,
    pub promised_tokens: u64,
}

/// Read the placements per rung.
///
/// The filters apply as `/breakdown` reads them, scoped to the fields a request row answers.
#[utoipa::path(
    get,
    path = "/admin/analytics/placements",
    operation_id = "analytics_placements",
    tag = "Analytics",
    params(PlacementsQuery),
    responses(
        (status = 200, description = "The rows over the range.", body = Placements),
        (status = 400, description = "For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\n`patterns are at most 256 characters, at most 32 of them`, for `user_agents`, `user_agent_groups` or `clients`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn placements(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (from, to) = match parse_range(q.get("range").map(String::as_str), (state.clock)()) {
        Ok(r) => r,
        Err(m) => return failure(StatusCode::BAD_REQUEST, &m),
    };
    let filter = filter_of(&q);
    if !phoebis_ledger::patterns_within_bounds(&filter.user_agents)
        || !phoebis_ledger::patterns_within_bounds(&filter.user_agent_groups)
        || !phoebis_ledger::patterns_within_bounds(&filter.clients)
    {
        return failure(StatusCode::BAD_REQUEST, phoebis_ledger::PATTERNS_TOO_LONG);
    }
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    let rows = match state
        .ledger
        .db()
        .read(move |c| phoebis_ledger::placements(c, &filter, &live, from, to))
        .await
    {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    let rows = rows
        .into_iter()
        .map(|r| PlacementRow {
            rung: r.rung,
            placements: r.placements,
            hits: r.hits,
            hit_share: (r.placements > 0).then(|| r.hits as f64 / r.placements as f64),
            cache_read_tokens: r.cache_read_tokens,
            promised_tokens: r.promised_tokens,
        })
        .collect();
    Json(Placements { from, to, rows }).into_response()
}

/// The three windows the router reads, each null until an hour's sample fills it, so the console reads the keys
/// whatever the hour holds. Any other window id the samples carry joins the hours that hold one alone.
fn base_windows() -> Map<String, Value> {
    [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE]
        .into_iter()
        .map(|w| (w.to_string(), Value::Null))
        .collect()
}

/// An hour's last sample of one window as the wire carries it. The lead is scored at the sample's own instant,
/// since the bucket is the sampler's 300-second floor and a reset minutes away would read wrong from it, and a
/// sample that reported the reset alone has no lead. The pace runs to the account's end in the registry's current
/// term, resolved at the sample's instant, since the ledger keeps no term history. The lead matches the router's for
/// a trial end and for the samples taken after a cancellation was recorded, and the older samples of the window that
/// holds a cancelled end re-read with it.
fn window_json(s: &HourlyWindowSample, term: Option<&SubscriptionTerm>) -> WindowSample {
    let sampled_at = s.observed_at.unwrap_or(s.bucket * 1_000);
    let end = term.and_then(|t| term::end_of(t, sampled_at));
    WindowSample {
        utilization: s.utilization,
        reset_at: s.reset_at,
        lead: s
            .utilization
            .map(|u| lead_over_pace(&s.window, u, s.reset_at, end, sampled_at)),
        sampled_at,
    }
}

/// The account rollups over the range.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnalyticsAccounts {
    pub from: i64,
    pub to: i64,
    pub accounts: Vec<AccountRollup>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountRollup {
    pub account_id: String,
    pub peak: RollupPeak,
    pub overage_seconds: u64,
    pub cooldown_seconds: u64,
    pub pins: RollupPins,
    pub hours: Vec<RollupHour>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollupPeak {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub seven_day_overage_included: Option<f64>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollupPins {
    pub created: u64,
    pub migrated_in: u64,
    pub migrated_out: u64,
}

/// One hour. An hour with samples and no attempt is listed with null peaks and zero counts.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollupHour {
    pub hour: i64,
    pub utilization_5h_max: Option<f64>,
    pub utilization_7d_max: Option<f64>,
    pub utilization_7d_oi_max: Option<f64>,
    pub overage_seconds: u64,
    pub cooldown_seconds: u64,
    pub pins_created: u64,
    pub pins_migrated_in: u64,
    pub pins_migrated_out: u64,
    // A map in insertion order, since the three base windows lead and the others follow as the samples add them,
    // which a sorted map would not keep. Its schema is `windows_schema`'s.
    #[schema(schema_with = windows_schema)]
    pub windows: Map<String, Value>,
}

/// An hour's `windows`: each value a `WindowSample` or null.
// utoipa 6.0.0 cannot write a map of nullable references from `value_type`, since its map schema takes no `oneOf`,
// so the schema is built here with the sample inline. `schema_with` drops the field's doc comment, so the field's
// description lives here too.
fn windows_schema() -> utoipa::openapi::schema::Object {
    use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Schema, Type};
    use utoipa::PartialSchema;
    ObjectBuilder::new()
        .schema_type(Type::Object)
        .additional_properties(Some(Schema::OneOf(
            OneOfBuilder::new()
                .item(WindowSample::schema())
                .item(ObjectBuilder::new().schema_type(Type::Null))
                .build(),
        )))
        .description(Some(
            "Keyed by the window ids the quota samples carry: `five_hour`, `seven_day` and `seven_day_fable` are \
             always present, null for a window the hour holds no sample of, and every other id, \
             `seven_day_overage_included` among the four the console reads, only in the hours that hold a sample of \
             it.",
        ))
        .build()
}

/// The hour's last sample of one window. `lead` is the utilization less the share of the window elapsed at the
/// sample's instant toward the window's reset, or toward the account's subscription end when that comes first, null
/// with the utilization for a sample that reported the reset alone.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowSample {
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
    pub lead: Option<f64>,
    pub sampled_at: i64,
}

/// Read the account rollups.
///
/// The request takes `range` alone, no `group_by` or filters.
#[utoipa::path(
    get,
    path = "/admin/analytics/accounts",
    operation_id = "analytics_accounts",
    tag = "Analytics",
    params(RangeQuery),
    responses(
        (status = 200, description = "The rollups over the range.", body = AnalyticsAccounts),
        (status = 400, description = "For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn accounts(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (from, to) = match parse_range(q.get("range").map(String::as_str), (state.clock)()) {
        Ok(r) => r,
        Err(m) => return failure(StatusCode::BAD_REQUEST, &m),
    };
    let rows = match state
        .ledger
        .db()
        .read(move |c| read_account_rollups(c, hour_of(from), hour_ceil(to)))
        .await
    {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    // The same hour bounds in seconds as the rollups read, so a sample never falls outside the hours answered.
    let samples = match state
        .ledger
        .db()
        .read(move |c| hourly_window_samples(c, hour_of(from), hour_ceil(to)))
        .await
    {
        Ok(samples) => samples,
        Err(e) => return ledger_failure(e),
    };
    type Hour = (AccountRollupRow, Map<String, Value>);
    let snapshot = state.router.snapshot();
    let mut by_account: BTreeMap<String, (AccountRollupRow, BTreeMap<i64, Hour>)> = BTreeMap::new();
    for row in rows {
        let entry = by_account.entry(row.account_id.clone()).or_insert_with(|| {
            (
                AccountRollupRow {
                    account_id: row.account_id.clone(),
                    ..Default::default()
                },
                BTreeMap::new(),
            )
        });
        entry.0.add(&row);
        let hour = row.hour;
        entry.1.insert(hour, (row, base_windows()));
    }
    for s in samples {
        let entry = by_account.entry(s.account_id.clone()).or_insert_with(|| {
            (
                AccountRollupRow {
                    account_id: s.account_id.clone(),
                    ..Default::default()
                },
                BTreeMap::new(),
            )
        });
        // An hour with samples and no attempt is listed with null peaks and zero counts, so the cards draw an idle
        // hour's reading while the peaks and the sums stay the attempts' own.
        let (_, windows) = entry.1.entry(s.hour).or_insert_with(|| {
            (
                AccountRollupRow {
                    hour: s.hour,
                    account_id: s.account_id.clone(),
                    ..Default::default()
                },
                base_windows(),
            )
        });
        windows.insert(
            s.window.clone(),
            serde_json::to_value(window_json(&s, snapshot.term(&s.account_id)))
                .expect("a window sample serializes"),
        );
    }
    let accounts = by_account
        .into_iter()
        .map(|(account_id, (sum, hours))| AccountRollup {
            account_id,
            peak: RollupPeak {
                five_hour: sum.utilization_5h_max,
                seven_day: sum.utilization_7d_max,
                seven_day_overage_included: sum.utilization_7d_oi_max,
            },
            overage_seconds: sum.overage_seconds,
            cooldown_seconds: sum.cooldown_seconds,
            pins: RollupPins {
                created: sum.pins_created,
                migrated_in: sum.pins_migrated_in,
                migrated_out: sum.pins_migrated_out,
            },
            hours: hours
                .into_values()
                .map(|(h, windows)| RollupHour {
                    hour: h.hour * 1_000,
                    utilization_5h_max: h.utilization_5h_max,
                    utilization_7d_max: h.utilization_7d_max,
                    utilization_7d_oi_max: h.utilization_7d_oi_max,
                    overage_seconds: h.overage_seconds,
                    cooldown_seconds: h.cooldown_seconds,
                    pins_created: h.pins_created,
                    pins_migrated_in: h.pins_migrated_in,
                    pins_migrated_out: h.pins_migrated_out,
                    windows,
                })
                .collect(),
        })
        .collect();
    Json(AnalyticsAccounts { from, to, accounts }).into_response()
}

/// The burn series per group: an API key account's tokens in dollars, and a subscription account's as the share of
/// its own 5-hour and 7-day window and of its model's own week, and as the 20x window equivalent of each. `unit` and
/// the shares answer under `group_by=account` and under any grouping whose `accounts` filter names exactly one
/// account, and read null otherwise, since shares of different accounts' windows do not add. The equivalents answer
/// under every grouping.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Burn {
    pub from: i64,
    pub to: i64,
    pub bucket_seconds: i64,
    /// `account`, `client`, `effort`, `key`, `kind`, `model`, `pool`, `request_class`, `session`, `speed`, or
    /// `user_agent`.
    #[schema(example = "account")]
    pub group_by: String,
    pub points: Vec<BurnPoint>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BurnPoint {
    pub at: i64,
    pub group: String,
    /// `window_share` for an oauth account, `usd` for an API key one. Null for an account no longer registered, and
    /// under every grouping but `account` unless the `accounts` filter names exactly one account.
    #[schema(example = "window_share")]
    pub unit: Option<String>,
    /// An API key account's estimated cost. It also carries a subscription account's fast-mode and overage spend,
    /// which draws usage credits and no window.
    pub usd: f64,
    /// The share of the account's own 5-hour window, band-weighted by the hour the tokens landed in: a fraction of
    /// the window, not a percentage. Null under every grouping but `account` unless the `accounts` filter names
    /// exactly one account, for an API key account, one whose tier is unknown, a bucket with no row the cost model
    /// prices, such as an idle hour, or a bucket holding a row the cost model cannot price (a model it weighs neither
    /// by itself nor by its model tier, or a tier it has no capacity for on that window), so a partial figure never
    /// reads as the bucket's share.
    pub five_hour_share: Option<f64>,
    /// The same tokens over the 7-day window, weighed by the week's own band factor, 1 in the shipped model. Null
    /// in the same cases.
    pub seven_day_share: Option<f64>,
    /// The same tokens over the model's own week, where the model has one. Null where no row of the bucket is on
    /// such a model, and in the same cases.
    pub model_week_share: Option<f64>,
    /// The bucket's plan-funded tokens as the share of a Max 20x account's 5-hour window, whatever tier served them,
    /// band-weighted as `five_hour_share` is, under every grouping. A billed row adds a zero, so a bucket with tokens
    /// and no plan row reads 0. Null for an idle bucket and for a bucket holding a row the cost model cannot price on
    /// the window.
    pub five_hour_equivalent: Option<f64>,
    /// The same over a Max 20x account's 7-day window. Null in the same cases.
    pub seven_day_equivalent: Option<f64>,
    /// The same over the model's own week, null where no row of the bucket is on a model with a week of its own.
    pub model_week_equivalent: Option<f64>,
    /// Whether a plan-funded row of the bucket priced at its model tier's latest weight, for a model the cost model
    /// does not weight.
    pub estimated: bool,
    /// True for a bucket holding a plan-funded row with tokens the cost model could not price on the 5-hour window,
    /// false for an idle bucket, so a null `five_hour_equivalent` with the flag false is an idle bucket. A window
    /// stays unpriced only for a model with no tier, or a tier with no weighted model.
    pub five_hour_unpriced: bool,
    /// The same on the 7-day window.
    pub seven_day_unpriced: bool,
    /// The same on the model's own week, false where no row of the bucket is on a model with a week of its own.
    pub model_week_unpriced: bool,
    pub cached_tokens: u64,
    /// Every token of the bucket, uncached input, cache reads, both cache writes and output. Zero for a bucket
    /// with no traffic the ledger can attribute.
    pub tokens: u64,
    pub uncached_tokens: u64,
}

/// Read the burn series.
#[utoipa::path(
    get,
    path = "/admin/analytics/burn",
    operation_id = "analytics_burn",
    tag = "Analytics",
    params(BurnQuery),
    responses(
        (status = 200, description = "The points over the range.", body = Burn),
        (status = 400, description = "For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\nFor the other parameters:\n- `group_by is account, client, effort, key, kind, model, pool, request_class, session, speed, or user_agent`\n- `patterns are at most 256 characters, at most 32 of them`, for `user_agent_groups`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
// The shares and `unit` are answered where each group holds one account's rows, the account grouping or a filter
// naming exactly one account, and read null otherwise, since shares of different accounts' windows do not add. A
// point whose shares answer knows its account's funding, so `unit` follows the same gate. The 20x equivalents add
// across accounts and tiers, so they answer under every grouping. A bucket holding a row the table cannot price
// reads null on that window, so a partial sum never reads as its share. Each point's `tokens` counts every token
// type of the bucket's rows, whatever the tables priced, so a reader tells an idle bucket from one whose traffic no
// table priced.
pub(crate) async fn burn(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let now = (state.clock)();
    let (from, to) = match parse_range(q.get("range").map(String::as_str), now) {
        Ok(r) => r,
        Err(m) => return failure(StatusCode::BAD_REQUEST, &m),
    };
    let group_by = q
        .get("group_by")
        .cloned()
        .unwrap_or_else(|| "account".into());
    if !BURN_GROUP_BYS.contains(&group_by.as_str()) {
        return failure(
            StatusCode::BAD_REQUEST,
            "group_by is account, client, effort, key, kind, model, pool, request_class, session, speed, or user_agent",
        );
    }
    let agent_groups = agent_list(&q, "user_agent_groups");
    if !phoebis_ledger::patterns_within_bounds(&agent_groups) {
        return failure(StatusCode::BAD_REQUEST, phoebis_ledger::PATTERNS_TOO_LONG);
    }
    let kinds = match kinds(&state) {
        Ok(kinds) => kinds,
        Err(r) => return r,
    };
    let accounts = list(&q, "accounts");
    let one_account = names_one_account(&accounts).then(|| accounts[0].clone());
    let tables = state.tables.clone();
    let bucket = bucket_seconds(from, to);
    let filter = SeriesFilter {
        from,
        to,
        bucket_seconds: bucket,
        accounts,
        ..Default::default()
    };
    let prices = state.pricing.revisions();
    let read = if group_by == "session" {
        state
            .ledger
            .db()
            .read(move |c| session_burn_series(c, &tables, &prices, &filter))
            .await
    } else {
        // One paged read for every group: each row is priced into its own group's buckets as its page arrives,
        // rather than the range read again per group under the reader lock.
        burn_groups_paged(
            state.ledger.db(),
            &tables,
            &prices,
            &filter,
            &group_by,
            &LiveKeys::from_snapshot(&state.router.snapshot()),
            &agent_groups,
        )
        .await
    };
    let series = match read {
        Ok(s) => s,
        Err(e) => return ledger_failure(e),
    };
    let points = series
        .into_iter()
        .flat_map(|(group, points)| {
            // The one account the group's rows belong to, under the gate the ledger answers the shares under. Its
            // kind names the unit a reader charts the group in, so a subscription whose tier no table sizes reads
            // apart from an api_key account, both answering no share. An account the registry no longer holds has
            // none.
            let account = if group_by == "account" {
                Some(&group)
            } else {
                one_account.as_ref()
            };
            let unit = account
                .and_then(|account| kinds.get(account))
                .map(|kind| match kind {
                    AccountKind::Oauth => "window_share",
                    AccountKind::ApiKey => "usd",
                });
            points.into_iter().map(move |p| BurnPoint {
                at: p.at,
                group: group.clone(),
                unit: unit.map(String::from),
                usd: p.usd,
                five_hour_share: p.five_hour_share,
                seven_day_share: p.seven_day_share,
                model_week_share: p.model_week_share,
                five_hour_equivalent: p.five_hour_equivalent,
                seven_day_equivalent: p.seven_day_equivalent,
                model_week_equivalent: p.model_week_equivalent,
                estimated: p.estimated,
                // The equivalents' latches, never the shares' `five_hour_unpriced` and `seven_day_unpriced`, which
                // latch only where the read prices shares.
                five_hour_unpriced: p.five_hour_equivalent_unpriced,
                seven_day_unpriced: p.seven_day_equivalent_unpriced,
                model_week_unpriced: p.model_week_equivalent_unpriced,
                cached_tokens: p.cached_tokens,
                tokens: p.tokens,
                uncached_tokens: p.uncached_tokens,
            })
        })
        .collect();
    Json(Burn {
        from,
        to,
        bucket_seconds: bucket,
        group_by,
        points,
    })
    .into_response()
}

/// The reconciled windows of the trailing 30 days, newest first.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reconciliations {
    pub reconciliations: Vec<Reconciliation>,
    /// The days the ledger keeps a request row, 0 for no limit. A window whose first reading is older finds no
    /// requests to price, so it is never reconciled.
    pub request_retention_days: u32,
}

/// One completed window of an account: the rise its readings showed against the share the router priced for its own
/// plan-funded attempts completed between those readings, each under its own regime.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reconciliation {
    pub account_id: String,
    /// `five_hour`, `seven_day` or `seven_day_fable`.
    #[schema(example = "five_hour")]
    pub window: String,
    /// The reset the window's readings named.
    pub window_end: i64,
    pub first_reading_at: i64,
    pub last_reading_at: i64,
    /// The last reading's utilization less the first's, as a fraction of the window.
    pub observed_rise: f64,
    /// The share of the window the router priced its attempts at.
    pub predicted_share: f64,
    /// `observed_rise` over `predicted_share`. A ratio under 0.8 or over 1.25 holds `meter_drift` while the row is its
    /// window's newest among those with a rise of at least 0.05, so a newer row with a smaller rise leaves it standing.
    pub ratio: f64,
    /// The attempts priced.
    pub requests: u32,
    /// The attempts among them priced at a model tier's estimate.
    pub estimated: u32,
    pub computed_at: i64,
}

/// Read the reconciled windows.
#[utoipa::path(
    get,
    path = "/admin/analytics/reconciliations",
    operation_id = "analytics_reconciliations",
    tag = "Analytics",
    params(ReconciliationsQuery),
    responses(
        (status = 200, description = "The windows that ended in the trailing 30 days, newest first.", body = Reconciliations),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn reconciliations(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let now = (state.clock)();
    let accounts = list(&q, "accounts");
    let read = state
        .ledger
        .db()
        .read(move |c| read_reconciliations(c, &accounts, now))
        .await;
    let rows = match read {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    Json(Reconciliations {
        reconciliations: rows
            .into_iter()
            .map(|r| Reconciliation {
                account_id: r.account_id,
                window: r.window,
                window_end: r.window_end,
                first_reading_at: r.first_reading_at,
                last_reading_at: r.last_reading_at,
                observed_rise: r.observed_rise,
                predicted_share: r.predicted_share,
                ratio: r.ratio,
                requests: r.requests,
                estimated: r.estimated,
                computed_at: r.computed_at,
            })
            .collect(),
        request_retention_days: state.ledger.retention_days(),
    })
    .into_response()
}

/// The rewrite tokens per bucket, the moves over the range and the slow-down's held time per bucket.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rewrites {
    pub from: i64,
    pub to: i64,
    pub bucket_seconds: i64,
    pub points: Vec<RewritePoint>,
    /// Move counts by reason, summed over the range: each reason's sum over its fits.
    pub moves: BTreeMap<String, u64>,
    /// Move counts by reason, then by the destination's fit (`fits`, `rewrite_only`, `least_bad`, or `unknown` for a
    /// move the fit gate did not read), summed over the range.
    pub moves_by_reason_and_fit: BTreeMap<String, BTreeMap<String, u64>>,
    /// The slow-down's held time per bucket: the milliseconds the requests of the bucket waited and how many waited.
    pub held: Vec<HeldPoint>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeldPoint {
    pub at: i64,
    /// The bucket's slow-down waits summed, the waits for a slot or a reset left out.
    pub held_ms: u64,
    /// The requests of the bucket that waited through a slow-down hold.
    pub requests: u64,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RewritePoint {
    pub at: i64,
    /// The cache-write tokens the first request after a move booked to the account rollups.
    pub rewrite_tokens: u64,
}

/// Read the rewrite tokens and the moves.
#[utoipa::path(
    get,
    path = "/admin/analytics/rewrites",
    operation_id = "analytics_rewrites",
    tag = "Analytics",
    params(RangeQuery),
    responses(
        (status = 200, description = "The points, the moves and the slow-down's held time over the range.", body = Rewrites),
        (status = 400, description = "For a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn rewrites(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (from, to) = match parse_range(q.get("range").map(String::as_str), (state.clock)()) {
        Ok(r) => r,
        Err(m) => return failure(StatusCode::BAD_REQUEST, &m),
    };
    let bucket = bucket_seconds(from, to);
    let filter = SeriesFilter {
        from,
        to,
        bucket_seconds: bucket,
        ..Default::default()
    };
    let read = state
        .ledger
        .db()
        .read(move |c| {
            Ok((
                rewrite_series(c, &filter)?,
                moves_by_reason_and_fit(c, &filter)?,
                held_series(c, &filter)?,
            ))
        })
        .await;
    let (series, moves_by_reason_and_fit, held) = match read {
        Ok(x) => x,
        Err(e) => return ledger_failure(e),
    };
    // The sum `moves_by_reason` answers, taken from the nested read so the rollups are read once.
    let moves = moves_by_reason_and_fit
        .iter()
        .map(|(reason, fits)| (reason.clone(), fits.values().sum()))
        .collect();
    Json(Rewrites {
        from,
        to,
        bucket_seconds: bucket,
        points: series
            .into_iter()
            .map(|(at, rewrite_tokens)| RewritePoint { at, rewrite_tokens })
            .collect(),
        moves,
        moves_by_reason_and_fit,
        held: held
            .into_iter()
            .map(|(at, held_ms, requests)| HeldPoint {
                at,
                held_ms,
                requests,
            })
            .collect(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        AnalyticsAccounts, Budgets, Burn, Placements, Reconciliations, Rewrites, WindowSample,
    };
    use crate::openapi::round_trip::{fixture, round_trips};
    use crate::openapi::{BreakdownRow, SeriesPointView};

    /// The breakdown with its rows typed as the mirror, so the whole body round trips.
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BreakdownTwin {
        from: i64,
        to: i64,
        group_by: String,
        rows: Vec<BreakdownRow>,
    }

    /// The series with its points typed as the mirror, so the whole body round trips.
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SeriesTwin {
        from: i64,
        to: i64,
        bucket_seconds: i64,
        group_by: String,
        points: Vec<SeriesPointView>,
    }

    #[test]
    fn the_analytics_fixtures_round_trip_through_their_declared_types_and_mirrors() {
        round_trips::<AnalyticsAccounts>("analytics_accounts");
        for stem in [
            "analytics_breakdown",
            "analytics_breakdown_accounts",
            "analytics_breakdown_speed",
        ] {
            round_trips::<BreakdownTwin>(stem);
        }
        round_trips::<Budgets>("analytics_budgets");
        round_trips::<Burn>("analytics_burn");
        round_trips::<Placements>("analytics_placements");
        round_trips::<Reconciliations>("analytics_reconciliations");
        round_trips::<Rewrites>("analytics_rewrites");
        for stem in ["analytics_series", "analytics_series_accounts"] {
            round_trips::<SeriesTwin>(stem);
        }
        // Each window is a sample or null, as `windows_schema` declares, so every window of every hour is read as
        // the nullable sample and the mirror is proved on the ones that hold one.
        let accounts = fixture("analytics_accounts");
        let mut sampled = 0;
        for (window, sample) in accounts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|a| a["hours"].as_array().unwrap())
            .flat_map(|hour| hour["windows"].as_object().unwrap())
        {
            let typed: Option<WindowSample> =
                serde_json::from_value(sample.clone()).unwrap_or_else(|e| panic!("{window}: {e}"));
            assert_eq!(serde_json::to_value(&typed).unwrap(), *sample, "{window}");
            sampled += typed.is_some() as usize;
        }
        assert!(
            sampled >= 1,
            "at least one window of one hour holds a sample, so WindowSample is exercised"
        );
    }
}
