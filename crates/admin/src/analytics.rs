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
    AccountRollupRow, LiveKeys, RollupFilter, RollupRow, SeriesFilter, Tiers, GROUP_BYS,
};
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
    let Ok(spends) = state.router.member_spends().await else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        );
    };
    let mut members: Vec<Value> = snapshot
        .budgeted_members()
        .map(|m| {
            let spend = crate::routes::member_spend_of(
                &spends,
                &m.pool_id,
                &m.account_id,
                m.budget_usd,
                m.budget_period,
            );
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

/// The burn series per group: a subscription account's tokens as the share of its own 5-hour and 7-day window,
/// each with its unmeasured flag, and an api_key account's in dollars. The shares, their flags and `unit` are
/// answered under the `account` grouping alone and read null under every other one, since a key, a class or a
/// session spans accounts and shares of different accounts' windows do not add.
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
    let mut tiers = Tiers::new();
    for account in accounts.iter().filter(|a| a.kind == AccountKind::Oauth) {
        let tier = match state.store.subscription(&account.id) {
            Ok(row) => row.and_then(|s| s.tier),
            Err(e) => return store_failure(e),
        };
        tiers.insert(account.id.clone(), tier);
    }
    let tables = state.tables.clone();
    let bucket = bucket_seconds(from, to);
    let (from_hour, to_hour) = (hour_of(from), hour_ceil(to));
    let group_by_read = group_by.clone();
    let kinds_read = kinds.clone();
    let read = state
        .ledger
        .db()
        .read(move |c| {
            let kinds = kinds_read;
            if group_by_read == "session" {
                let filter = SeriesFilter {
                    from,
                    to,
                    bucket_seconds: bucket,
                    ..Default::default()
                };
                return session_burn_series(c, &tables, &kinds, &tiers, &filter);
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
                let points = burn_series_over(&rows, &tables, &kinds, &tiers, &filter)?;
                out.push((group, points));
            }
            Ok(out)
        })
        .await;
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
                let (five_hour_unmeasured, seven_day_unmeasured) = if by_account {
                    (Some(p.five_hour_unmeasured), Some(p.seven_day_unmeasured))
                } else {
                    (None, None)
                };
                json!({
                    "at": p.at, "group": group, "unit": unit, "usd": p.usd,
                    "five_hour_share": p.five_hour_share.filter(|_| by_account), "five_hour_unmeasured": five_hour_unmeasured,
                    "seven_day_share": p.seven_day_share.filter(|_| by_account), "seven_day_unmeasured": seven_day_unmeasured,
                    "cached_tokens": p.cached_tokens, "uncached_tokens": p.uncached_tokens,
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

/// How far back the reconciliation table reads: a month of 5-hour windows and four 7-day ones.
const RECONCILIATIONS_MS: i64 = 30 * 86_400_000;

/// The windows reconciled over the last 30 days, newest first, beside the request retention as configured, so the
/// page can say which windows the retention leaves unreconciled.
pub(crate) async fn reconciliations(State(state): State<Arc<AdminState>>) -> Response {
    let since = (state.clock)() - RECONCILIATIONS_MS;
    let rows = match state.ledger.db().reconciliations(since).await {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    Json(json!({
        "reconciliations": rows,
        "request_retention_days": state.settings.retention_days,
    }))
    .into_response()
}
