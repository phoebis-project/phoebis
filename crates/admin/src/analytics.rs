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
use serde_json::{json, Map, Value};

use crate::routes::{failure, store_failure};
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
    let mut tiers = Tiers::new();
    if group_by == "account" {
        for account in accounts.iter().filter(|a| a.kind == AccountKind::Oauth) {
            let tier = state
                .store
                .subscription(&account.id)
                .map_err(store_failure)?
                .and_then(|s| s.tier);
            tiers.insert(account.id.clone(), tier);
        }
    }
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

pub(crate) fn ledger_failure(error: phoebis_ledger::LedgerError) -> Response {
    tracing::warn!(%error, "an admin read failed in the ledger");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "ledger failure")
}

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
    Json(json!({ "from": from, "to": to, "bucket_seconds": bucket, "group_by": group_by, "points": points })).into_response()
}

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
    Json(json!({ "from": from, "to": to, "group_by": group_by, "rows": rows })).into_response()
}

pub(crate) async fn budgets(State(state): State<Arc<AdminState>>) -> Response {
    let snapshot = state.router.snapshot();
    let Ok(spends) = state.router.member_spends().await else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        );
    };
    // One row per budgeted period, so a member with both budgets reads twice, its day before its month.
    let mut members: Vec<(String, String, phoebis_store::BudgetPeriod, Value)> = snapshot
        .budgeted_members()
        .flat_map(|m| {
            let rows = crate::routes::spend_rows(&spends, &m.pool_id, &m.account_id);
            let kind = snapshot
                .account(&m.account_id)
                .map(|a| crate::routes::kind_name(a.kind));
            m.budgets()
                .map(|(period, budget)| {
                    let spend = rows.iter().find(|s| s.period == period);
                    let row = json!({
                        "pool_id": m.pool_id, "account_id": m.account_id, "kind": kind,
                        "budget_usd": budget, "budget_period": period.as_str(),
                        "spent_usd": spend.map(|s| s.spent_usd), "period_end": spend.map(|s| s.period_end),
                    });
                    (m.pool_id.clone(), m.account_id.clone(), period, row)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let month =
        |period: &phoebis_store::BudgetPeriod| *period == phoebis_store::BudgetPeriod::Month;
    members.sort_by(|a, b| (&a.0, &a.1, month(&a.2)).cmp(&(&b.0, &b.1, month(&b.2))));
    let members: Vec<Value> = members.into_iter().map(|(.., row)| row).collect();
    Json(json!({ "members": members })).into_response()
}

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
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "rung": r.rung, "placements": r.placements, "hits": r.hits,
                "hit_share": (r.placements > 0).then(|| r.hits as f64 / r.placements as f64),
                "cache_read_tokens": r.cache_read_tokens, "promised_tokens": r.promised_tokens,
            })
        })
        .collect();
    Json(json!({ "from": from, "to": to, "rows": rows })).into_response()
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
fn window_json(s: &HourlyWindowSample) -> Value {
    let sampled_at = s.observed_at.unwrap_or(s.bucket * 1_000);
    json!({
        "utilization": s.utilization,
        "reset_at": s.reset_at,
        "lead": s.utilization.map(|u| lead_over_pace(&s.window, u, s.reset_at, sampled_at)),
        "sampled_at": sampled_at,
    })
}

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
        windows.insert(s.window.clone(), window_json(&s));
    }
    let accounts: Vec<_> = by_account.into_iter().map(|(id, (sum, hours))| json!({
        "account_id": id,
        "peak": { "five_hour": sum.utilization_5h_max, "seven_day": sum.utilization_7d_max, "seven_day_overage_included": sum.utilization_7d_oi_max },
        "overage_seconds": sum.overage_seconds, "cooldown_seconds": sum.cooldown_seconds,
        "pins": { "created": sum.pins_created, "migrated_in": sum.pins_migrated_in, "migrated_out": sum.pins_migrated_out },
        "hours": hours.into_values().map(|(h, windows)| json!({ "hour": h.hour * 1_000, "utilization_5h_max": h.utilization_5h_max, "utilization_7d_max": h.utilization_7d_max, "utilization_7d_oi_max": h.utilization_7d_oi_max, "overage_seconds": h.overage_seconds, "cooldown_seconds": h.cooldown_seconds, "pins_created": h.pins_created, "pins_migrated_in": h.pins_migrated_in, "pins_migrated_out": h.pins_migrated_out, "windows": windows })).collect::<Vec<_>>(),
    })).collect();
    Json(json!({ "from": from, "to": to, "accounts": accounts })).into_response()
}

/// The burn series per group: a subscription account's tokens as the share of its own 5-hour and 7-day window,
/// and an api_key account's in dollars. The shares and `unit` are answered under the `account` grouping alone and
/// read null under every other one, since a key, a class or a session spans accounts and shares of different
/// accounts' windows do not add; a bucket holding a row the table cannot price reads null on that window, so a
/// partial sum never reads as its share. Each point's `tokens` counts every token type, zero for a bucket with no
/// traffic the ledger can attribute (a deregistered account's traffic adds none), so for a registered account a
/// reader tells an empty bucket from one whose traffic no table priced.
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
        let kinds = kinds.clone();
        state
            .ledger
            .db()
            .read(move |c| session_burn_series(c, &tables, &kinds, &tiers, &filter))
            .await
    } else {
        // One paged read for every group: each row is priced into its own group's buckets as its page arrives,
        // rather than the range read again per group under the reader lock.
        burn_groups_paged(
            state.ledger.db(),
            &tables,
            &kinds,
            &tiers,
            &filter,
            &group_by,
        )
        .await
    };
    let series = match read {
        Ok(s) => s,
        Err(e) => return ledger_failure(e),
    };
    let by_account = group_by == "account";
    let points: Vec<Value> = series
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
            points.into_iter().map(move |p| {
                json!({
                    "at": p.at, "group": group, "unit": unit, "usd": p.usd,
                    "five_hour_share": p.five_hour_share.filter(|_| by_account),
                    "seven_day_share": p.seven_day_share.filter(|_| by_account),
                    "cached_tokens": p.cached_tokens, "tokens": p.tokens, "uncached_tokens": p.uncached_tokens,
                })
            })
        })
        .collect();
    Json(json!({ "from": from, "to": to, "bucket_seconds": bucket, "group_by": group_by, "points": points })).into_response()
}

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
    Json(json!({
        "from": from, "to": to, "bucket_seconds": bucket,
        "points": series.into_iter().map(|(at, rewrite_tokens)| json!({ "at": at, "rewrite_tokens": rewrite_tokens })).collect::<Vec<_>>(),
        "moves": moves,
    }))
    .into_response()
}
