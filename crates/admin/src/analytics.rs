//! Rollup reads: the series bucketed to the range, the breakdown per group, and the account rollups.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::{
    apply_filter, bucket_seconds, hour_ceil, hour_of, parse_range, read_account_rollups,
    read_rollups, AccountRollupRow, LiveKeys, RollupFilter, RollupRow, GROUP_BYS,
};
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

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
        clients: Vec::new(),
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
            "group_by is account, all, effort, key, kind, model, pool, or user_agent",
        ));
    }
    let filter = filter_of(q);
    if !phoebis_ledger::patterns_within_bounds(&filter.user_agents)
        || !phoebis_ledger::patterns_within_bounds(&filter.user_agent_groups)
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
