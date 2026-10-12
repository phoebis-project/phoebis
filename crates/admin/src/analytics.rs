//! Rollup reads: the series bucketed to the range, the breakdown per group, and the account rollups.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    breakdown_paged, bucket_seconds, burn_groups_paged, hour_ceil, hour_of, hourly_window_samples,
    lead_over_pace, moves_by_reason, parse_range, read_account_rollups, rewrite_series,
    series_paged, session_burn_series, AccountRollupRow, FundingFilter, HourlyWindowSample,
    LedgerError, LiveKeys, RollupFilter, SeriesFilter, SharePricing, Tiers, GROUP_BYS,
};
use phoebis_routing::{FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::AccountKind;
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::openapi::{ErrorBody, LedgerFailure, RouterUnavailable};
use crate::routes::{failure, store_failure};
use crate::tiers::oauth_tiers;
use crate::AdminState;

const BURN_GROUP_BYS: &[&str] = &["account", "key", "request_class", "session"];

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
/// against, the registry's account kinds and, under the account grouping, each oauth account's tier.
type Selection = (
    (i64, i64),
    String,
    RollupFilter,
    LiveKeys,
    HashMap<String, AccountKind>,
    Tiers,
);

/// The registry's account kinds and, under the account grouping alone, each oauth account's tier, which the shares
/// price under: no other grouping answers a share, so none reads a tier or fails on that read.
#[allow(clippy::result_large_err)]
fn kinds_and_tiers(
    state: &AdminState,
    group_by: &str,
) -> Result<(HashMap<String, AccountKind>, Tiers), Response> {
    let accounts = state.store.accounts().map_err(store_failure)?;
    let kinds = accounts.iter().map(|a| (a.id.clone(), a.kind)).collect();
    let tiers = if group_by == "account" {
        oauth_tiers(&state.store, &accounts).map_err(store_failure)?
    } else {
        Tiers::new()
    };
    Ok((kinds, tiers))
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
    let (kinds, tiers) = kinds_and_tiers(state, &group_by)?;
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    Ok(((from, to), group_by, filter, live, kinds, tiers))
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
    /// Default `24h`.
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
    /// Default `24h`.
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
    /// Default `24h`.
    pub range: Option<String>,
    /// `account`, `key`, `request_class`, or `session`. Default `account`. The `session` grouping reads the twenty
    /// sessions with the most tokens over the range, since the hourly rollups carry no session dimension. The rest
    /// read the hourly rollups the way `breakdown` and `series` do.
    pub group_by: Option<String>,
}

pub(crate) fn ledger_failure(error: phoebis_ledger::LedgerError) -> Response {
    tracing::warn!(%error, "an admin read failed in the ledger");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "ledger failure")
}

/// The series bucketed to the range. Under `group_by=key` a point's `groups` entries carry `key` in the same form
/// as the breakdown route's key row. Under `group_by=account` each entry carries the breakdown row's
/// `cost_shares`, `cache_shares`, `probe_shares`, `plan_requests` and `plan_output_tokens` under the same rule,
/// summed over the bucket's rows.
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
        (status = 400, description = "- `funding is billed or plan`\n- `group_by is account, all, client, effort, key, kind, model, pool, request_class, speed, or user_agent`\n- `patterns are at most 256 characters, at most 32 of them`, for `user_agents`, `user_agent_groups` or `clients`\n\nFor a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`\n\nFor a grouping error, the ledger's own message.", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn series(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ((from, to), group_by, filter, live, kinds, tiers) = match selection(&state, &q).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let bucket = bucket_seconds(from, to);
    let pricing = (group_by == "account").then(|| SharePricing {
        tables: &state.tables,
        kinds: &kinds,
        tiers: &tiers,
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
        (status = 400, description = "- `funding is billed or plan`\n- `group_by is account, all, client, effort, key, kind, model, pool, request_class, speed, or user_agent`\n- `patterns are at most 256 characters, at most 32 of them`, for `user_agents`, `user_agent_groups` or `clients`\n\nFor a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
pub(crate) async fn breakdown(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ((from, to), group_by, filter, live, kinds, tiers) = match selection(&state, &q).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let pricing = (group_by == "account").then(|| SharePricing {
        tables: &state.tables,
        kinds: &kinds,
        tiers: &tiers,
    });
    let rows = match breakdown_paged(
        state.ledger.db(),
        &filter,
        &live,
        from,
        to,
        &group_by,
        pricing,
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
        (status = 400, description = "- `patterns are at most 256 characters, at most 32 of them`, for `user_agents`, `user_agent_groups` or `clients`\n\nFor a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
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
/// sample that reported the reset alone has no lead.
fn window_json(s: &HourlyWindowSample) -> WindowSample {
    let sampled_at = s.observed_at.unwrap_or(s.bucket * 1_000);
    WindowSample {
        utilization: s.utilization,
        reset_at: s.reset_at,
        lead: s
            .utilization
            .map(|u| lead_over_pace(&s.window, u, s.reset_at, sampled_at)),
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
/// sample's instant, null with the utilization for a sample that reported the reset alone.
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
            serde_json::to_value(window_json(&s)).expect("a window sample serializes"),
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

/// The burn series per group: a subscription account's tokens as the share of its own 5-hour and 7-day window,
/// and an API key account's in dollars. `unit` and the two shares are a `group_by=account` figure, null for every
/// other grouping, since a key, a class or a session spans accounts whose shares do not add.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Burn {
    pub from: i64,
    pub to: i64,
    pub bucket_seconds: i64,
    /// `account`, `key`, `request_class`, or `session`.
    #[schema(example = "account")]
    pub group_by: String,
    pub points: Vec<BurnPoint>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BurnPoint {
    pub at: i64,
    pub group: String,
    /// `window_share` for an oauth account, `usd` for an API key one, null for an account no longer registered.
    #[schema(example = "window_share")]
    pub unit: Option<String>,
    /// An API key account's estimated cost. It also carries a subscription account's fast-mode and overage spend,
    /// which draws usage credits and no window.
    pub usd: f64,
    /// The share of the account's own 5-hour window, band-weighted by the hour the tokens landed in: a fraction of
    /// the window, not a percentage. Null for an API key account, one whose tier is unknown, a bucket with no row
    /// the cost model prices, such as an idle hour, or a bucket holding a row the cost model cannot price (a model
    /// it has no weight for, or a tier it has no capacity for on that window), so a partial figure never reads as
    /// the bucket's share.
    pub five_hour_share: Option<f64>,
    /// The same tokens over the 7-day window, weighed by the week's own band factor, 1 in the shipped model. Null
    /// in the same four cases.
    pub seven_day_share: Option<f64>,
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
        (status = 400, description = "- `group_by is account, key, request_class, or session`\n\nFor a bad `range`:\n- `range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds`\n- `from is not a number`\n- `to is not a number`\n- `from must be before to`\n- `to is past the ledger's lifetime`\n- `a custom range spans at most 400 days`", body = ErrorBody, example = json!({"error": "range is 1h, 24h, 7d, 30d, or <from>,<to> in Unix milliseconds"})),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
    ),
)]
// The shares and `unit` are answered under the `account` grouping alone and read null under every other one, since
// a key, a class or a session spans accounts and shares of different accounts' windows do not add. A bucket holding
// a row the table cannot price reads null on that window, so a partial sum never reads as its share. Each point's
// `tokens` counts every token type, zero for a bucket with no traffic the ledger can attribute (a deregistered
// account's traffic adds none), so for a registered account a reader tells an empty bucket from one whose traffic no
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
            "group_by is account, key, request_class, or session",
        );
    }
    let (kinds, tiers) = match kinds_and_tiers(&state, &group_by) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let tables = state.tables.clone();
    let bucket = bucket_seconds(from, to);
    let filter = SeriesFilter {
        from,
        to,
        bucket_seconds: bucket,
        ..Default::default()
    };
    let read = if group_by == "session" {
        state
            .ledger
            .db()
            .read(move |c| session_burn_series(c, &tables, &tiers, &filter))
            .await
    } else {
        // One paged read for every group: each row is priced into its own group's buckets as its page arrives,
        // rather than the range read again per group under the reader lock.
        burn_groups_paged(state.ledger.db(), &tables, &tiers, &filter, &group_by).await
    };
    let series = match read {
        Ok(s) => s,
        Err(e) => return ledger_failure(e),
    };
    let by_account = group_by == "account";
    let points = series
        .into_iter()
        .flat_map(|(group, points)| {
            // The unit a reader charts the group in, so a subscription whose tier no table sizes reads apart from
            // an api_key account, both answering no share; an account the registry no longer holds has none.
            let unit = by_account
                .then(|| kinds.get(&group))
                .flatten()
                .map(|kind| match kind {
                    AccountKind::Oauth => "window_share",
                    AccountKind::ApiKey => "usd",
                });
            points.into_iter().map(move |p| BurnPoint {
                at: p.at,
                group: group.clone(),
                unit: unit.map(String::from),
                usd: p.usd,
                five_hour_share: p.five_hour_share.filter(|_| by_account),
                seven_day_share: p.seven_day_share.filter(|_| by_account),
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

/// The rewrite tokens per bucket and the moves over the range.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rewrites {
    pub from: i64,
    pub to: i64,
    pub bucket_seconds: i64,
    pub points: Vec<RewritePoint>,
    /// Move counts by reason, summed over the range.
    pub moves: BTreeMap<String, u64>,
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
        (status = 200, description = "The points and the moves over the range.", body = Rewrites),
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
        .read(move |c| Ok((rewrite_series(c, &filter)?, moves_by_reason(c, &filter)?)))
        .await;
    let (series, moves): (Vec<(i64, u64)>, std::collections::BTreeMap<String, u64>) = match read {
        Ok(x) => x,
        Err(e) => return ledger_failure(e),
    };
    Json(Rewrites {
        from,
        to,
        bucket_seconds: bucket,
        points: series
            .into_iter()
            .map(|(at, rewrite_tokens)| RewritePoint { at, rewrite_tokens })
            .collect(),
        moves,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{AnalyticsAccounts, Budgets, Burn, Placements, Rewrites, WindowSample};
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
        round_trips::<Rewrites>("analytics_rewrites");
        for stem in ["analytics_series", "analytics_series_accounts"] {
            round_trips::<SeriesTwin>(stem);
        }
        // Three of the fixture's four hours carry a null `five_hour`, so every hour is read as the nullable sample
        // and the mirror is proved on the ones that hold one.
        let accounts = fixture("analytics_accounts");
        let mut sampled = 0;
        for hour in accounts["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|a| a["hours"].as_array().unwrap())
        {
            let sample = &hour["windows"]["five_hour"];
            let typed: Option<WindowSample> = serde_json::from_value(sample.clone()).unwrap();
            assert_eq!(serde_json::to_value(&typed).unwrap(), *sample);
            sampled += typed.is_some() as usize;
        }
        assert!(
            sampled >= 1,
            "at least one hour samples five_hour, so WindowSample is exercised"
        );
    }
}
