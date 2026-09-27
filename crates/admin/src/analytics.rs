//! Rollup reads: the series bucketed to the range, the breakdown per group, and the account rollups.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    apply_filter, bucket_seconds, burn_rows, burn_series_over, hour_ceil, hour_of, moves_by_reason,
    parse_range, read_account_rollups, read_rollups, rewrite_series, session_burn_series,
    AccountRollupRow, LiveKeys, PlanFactors, PlanWindows, RollupFilter, RollupRow, SeriesFilter,
    GROUP_BYS,
};
use phoebis_routing::FIVE_HOUR;
use phoebis_store::AccountKind;
use serde_json::{json, Value};

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

/// The same list, split on newlines instead of commas: every measured client agent carries a comma
/// (`claude-cli/2.1.269 (external, sdk-cli)`), and a pattern of the wildcard grammar carries no newline either.
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
    }
}

/// The range in Unix milliseconds, the group-by, whether probe rows count as requests, the rows under the
/// filter, the live keys' generations the key dimension is read against, and the agent pattern groups.
type Selection = (
    (i64, i64),
    String,
    bool,
    Vec<RollupRow>,
    LiveKeys,
    Vec<String>,
);

/// Every analytics route starts here.
// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
async fn rows_for(state: &AdminState, q: &HashMap<String, String>) -> Result<Selection, Response> {
    let (from, to) = parse_range(q.get("range").map(String::as_str), (state.clock)())
        .map_err(|m| failure(StatusCode::BAD_REQUEST, &m))?;
    let group_by = q.get("group_by").cloned().unwrap_or_else(|| "all".into());
    if !GROUP_BYS.contains(&group_by.as_str()) {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "group_by is account, all, client, effort, key, kind, model, pool, request_class, or user_agent",
        ));
    }
    let filter = filter_of(q);
    if !phoebis_ledger::patterns_within_bounds(&filter.user_agents)
        || !phoebis_ledger::patterns_within_bounds(&filter.user_agent_groups)
        || !phoebis_ledger::patterns_within_bounds(&filter.clients)
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            phoebis_ledger::PATTERNS_TOO_LONG,
        ));
    }
    let (from_hour, to_hour) = (hour_of(from), hour_ceil(to));
    let rows = state
        .ledger
        .db()
        .read(move |c| read_rollups(c, from_hour, to_hour))
        .await
        .map_err(ledger_failure)?;
    let live = LiveKeys::from_snapshot(&state.router.snapshot());
    // Cloned rather than moved: `filter` still owes `apply_filter` a borrow below, so the group list needs
    // its own copy to ride along as the `Selection`'s sixth member.
    let agent_groups = filter.user_agent_groups.clone();
    Ok((
        (from, to),
        group_by,
        filter.probes_counted(),
        apply_filter(rows, &filter, &live),
        live,
        agent_groups,
    ))
}

pub(crate) fn ledger_failure(error: phoebis_ledger::LedgerError) -> Response {
    tracing::warn!(%error, "an admin read failed in the ledger");
    failure(StatusCode::INTERNAL_SERVER_ERROR, "ledger failure")
}

pub(crate) async fn series(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ((from, to), group_by, probes_counted, rows, live, agent_groups) =
        match rows_for(&state, &q).await {
            Ok(x) => x,
            Err(r) => return r,
        };
    let bucket = bucket_seconds(from, to);
    let points = match phoebis_ledger::series(
        &rows,
        from,
        to,
        bucket,
        &group_by,
        probes_counted,
        &live,
        &agent_groups,
    ) {
        Ok(p) => p,
        Err(e) => return failure(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    Json(json!({ "from": from, "to": to, "bucket_seconds": bucket, "group_by": group_by, "points": points })).into_response()
}

pub(crate) async fn breakdown(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ((from, to), group_by, probes_counted, rows, live, agent_groups) =
        match rows_for(&state, &q).await {
            Ok(x) => x,
            Err(r) => return r,
        };
    Json(json!({ "from": from, "to": to, "group_by": group_by, "rows": phoebis_ledger::breakdown(&rows, &group_by, probes_counted, &live, &agent_groups) })).into_response()
}

pub(crate) async fn budgets(State(state): State<Arc<AdminState>>) -> Response {
    let snapshot = state.router.snapshot();
    let spends = state.router.member_spends().await.unwrap_or_default();
    let mut members: Vec<Value> = snapshot
        .budgeted_members()
        .map(|m| {
            let spend = spends
                .iter()
                .find(|s| s.pool_id == m.pool_id && s.account_id == m.account_id);
            json!({
                "pool_id": m.pool_id, "account_id": m.account_id,
                "budget_usd": m.budget_usd, "budget_period": m.budget_period.as_str(),
                "spent_usd": spend.map(|s| s.spent_usd), "period_end": spend.map(|s| s.period_end),
            })
        })
        .collect();
    members.sort_by(|a, b| {
        (a["pool_id"].as_str(), a["account_id"].as_str())
            .cmp(&(b["pool_id"].as_str(), b["account_id"].as_str()))
    });
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
    let mut by_account: std::collections::BTreeMap<
        String,
        (AccountRollupRow, Vec<AccountRollupRow>),
    > = std::collections::BTreeMap::new();
    for row in rows {
        let entry = by_account.entry(row.account_id.clone()).or_insert_with(|| {
            (
                AccountRollupRow {
                    account_id: row.account_id.clone(),
                    ..Default::default()
                },
                Vec::new(),
            )
        });
        entry.0.add(&row);
        entry.1.push(row);
    }
    let accounts: Vec<_> = by_account.into_iter().map(|(id, (sum, hours))| json!({
        "account_id": id,
        "peak": { "5h": sum.utilization_5h_max, "7d": sum.utilization_7d_max, "7d_oi": sum.utilization_7d_oi_max },
        "overage_seconds": sum.overage_seconds, "cooldown_seconds": sum.cooldown_seconds,
        "pins": { "created": sum.pins_created, "migrated_in": sum.pins_migrated_in, "migrated_out": sum.pins_migrated_out },
        "hours": hours.iter().map(|h| json!({ "hour": h.hour * 1_000, "utilization_5h_max": h.utilization_5h_max, "utilization_7d_max": h.utilization_7d_max, "utilization_7d_oi_max": h.utilization_7d_oi_max, "overage_seconds": h.overage_seconds, "cooldown_seconds": h.cooldown_seconds, "pins_created": h.pins_created, "pins_migrated_in": h.pins_migrated_in, "pins_migrated_out": h.pins_migrated_out })).collect::<Vec<_>>(),
    })).collect();
    Json(json!({ "from": from, "to": to, "accounts": accounts })).into_response()
}

/// The account's fitted 5-hour scale, else its predicted one, else `None`: the denominator `burn`'s
/// per-account `plan_percent` divides a bucket's meter tokens by.
fn plan_scale(
    state: &AdminState,
    fits: &[phoebis_store::FitRow],
    windows: &PlanWindows,
    account_id: &str,
) -> Option<f64> {
    if let Some(fitted) = fits
        .iter()
        .find(|f| f.account_id == account_id && f.window == FIVE_HOUR)
    {
        return Some(fitted.scale);
    }
    let tier = state.store.subscription(account_id).ok().flatten()?.tier?;
    windows.predicted(&tier, FIVE_HOUR)
}

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
    let accounts = match state.store.accounts() {
        Ok(a) => a,
        Err(e) => return store_failure(e),
    };
    let kinds: HashMap<String, AccountKind> =
        accounts.iter().map(|a| (a.id.clone(), a.kind)).collect();
    let fits = state.router.fits().await.unwrap_or_default();
    let bucket = bucket_seconds(from, to);
    let (from_hour, to_hour) = (hour_of(from), hour_ceil(to));
    let group_by_read = group_by.clone();
    let read = state
        .ledger
        .db()
        .read(move |c| {
            let factors = PlanFactors::shipped();
            let windows = PlanWindows::shipped();
            if group_by_read == "session" {
                let filter = SeriesFilter {
                    from,
                    to,
                    bucket_seconds: bucket,
                    ..Default::default()
                };
                return session_burn_series(c, &factors, &windows, &kinds, &filter);
            }
            let base = SeriesFilter {
                from,
                to,
                bucket_seconds: bucket,
                ..Default::default()
            };
            // One read for every group: each folds its own scope from these rows rather than reading the range
            // again under the reader lock. The groups are the ones the range itself holds; the read reaches back
            // to the first bucket's edge, which may start before it.
            let rows = burn_rows(c, &base)?;
            let mut groups: Vec<String> = rows
                .iter()
                .filter(|r| r.hour >= from_hour && r.hour < to_hour)
                .map(|r| match group_by_read.as_str() {
                    "key" => r.key_id.clone(),
                    "request_class" => r.request_class.clone(),
                    _ => r.account_id.clone(),
                })
                .collect();
            groups.sort();
            groups.dedup();
            let mut out = Vec::new();
            for group in groups {
                let one = vec![group.clone()];
                let filter = SeriesFilter {
                    accounts: if group_by_read == "account" {
                        one.clone()
                    } else {
                        Vec::new()
                    },
                    keys: if group_by_read == "key" {
                        one.clone()
                    } else {
                        Vec::new()
                    },
                    request_classes: if group_by_read == "request_class" {
                        one
                    } else {
                        Vec::new()
                    },
                    ..base.clone()
                };
                let points = burn_series_over(&rows, &factors, &windows, &kinds, &filter)?;
                out.push((group, points));
            }
            Ok(out)
        })
        .await;
    let series = match read {
        Ok(s) => s,
        Err(e) => return ledger_failure(e),
    };
    let windows = PlanWindows::shipped();
    let points: Vec<Value> = series
        .into_iter()
        .flat_map(|(group, points)| {
            // The scale is one per group, read once rather than once per point.
            let scale = (group_by == "account")
                .then(|| plan_scale(&state, &fits, &windows, &group))
                .flatten();
            let group_for_points = group.clone();
            points.into_iter().map(move |p| {
                json!({
                    "at": p.at, "group": group_for_points, "meter_tokens": p.meter_tokens, "usd": p.usd,
                    "cached_tokens": p.cached_tokens, "uncached_tokens": p.uncached_tokens,
                    "plan_percent": scale.filter(|s| *s > 0.0).map(|s| p.meter_tokens / s),
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

pub(crate) async fn fits(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let Ok(fits) = state.router.fits().await else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        );
    };
    let accounts = match state.store.accounts() {
        Ok(a) => a,
        Err(e) => return store_failure(e),
    };
    let windows = PlanWindows::shipped();
    let tier_by_account: HashMap<String, Option<String>> = accounts
        .iter()
        .filter(|a| a.kind == AccountKind::Oauth)
        .map(|a| {
            (
                a.id.clone(),
                state
                    .store
                    .subscription(&a.id)
                    .ok()
                    .flatten()
                    .and_then(|s| s.tier),
            )
        })
        .collect();
    let rows: Vec<Value> = fits
        .iter()
        .map(|f| {
            let tier = tier_by_account.get(&f.account_id).cloned().flatten();
            let predicted = tier.as_deref().and_then(|t| windows.predicted(t, &f.window));
            let drift = predicted.filter(|p| *p != 0.0).map(|p| f.scale / p);
            let band_factor_predicted = windows.band_factor_of(&f.window, now);
            // A provisional tolerance until the owner's next measurements tighten the row; no alert fires on
            // it, the table only shows it.
            let band_divergent = matches!(
                (f.band_factor, band_factor_predicted),
                (Some(fitted), Some(predicted)) if (fitted - predicted).abs() > predicted.abs() * 0.15
            );
            json!({
                "account_id": f.account_id, "window": f.window, "scale": f.scale, "confidence": f.confidence,
                "intervals": f.intervals,
                "corrections": serde_json::from_str::<Value>(&f.corrections_json).unwrap_or_else(|_| json!({})),
                "fitted_at": f.fitted_at, "predicted": predicted, "drift": drift,
                "band_factor": f.band_factor, "band_factor_predicted": band_factor_predicted,
                "band_divergent": band_divergent,
            })
        })
        .collect();
    Json(json!({ "fits": rows })).into_response()
}
