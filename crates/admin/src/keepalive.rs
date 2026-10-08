//! The keepalive views: every retained recipe, and the overview tiles the sessions page shows.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_keepalive::{IdleReturns, Wasted, IDLE_AGE_BOUNDS_SECONDS};
use phoebis_ledger::{hour_of, read_rollups, RollupSelect, UNATTRIBUTED};
use serde_json::json;

use crate::analytics::ledger_failure;
use crate::AdminState;

pub(crate) async fn recipes(State(state): State<Arc<AdminState>>) -> Response {
    Json(json!({ "recipes": state.keepalive.recipes() })).into_response()
}

#[derive(Default)]
struct PoolDay {
    credited: u64,
    probes: u64,
    wasted: u64,
    wasted_reasons: BTreeMap<String, u64>,
}

pub(crate) async fn overview(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let day = hour_of(now) - 24 * 3_600;
    let to = hour_of(now) + 3_600;
    let rows = match state
        .ledger
        .db()
        .read(move |c| read_rollups(c, day, to, &RollupSelect::default()))
        .await
    {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    let mut expired = 0u64;
    let mut probes = phoebis_ledger::ProbeCounts::default();
    let mut pools: BTreeMap<String, PoolDay> = BTreeMap::new();
    for row in &rows {
        expired += row
            .cache_miss_reasons
            .get("expired_between_turns")
            .copied()
            .unwrap_or(0);
        if row.kind == "probe" {
            probes.ok += row.succeeded;
            probes.failed +=
                row.failed + row.rate_limited + row.exhausted + row.rejected + row.incomplete;
            probes.migrated += row.probe_migrated;
            probes.skipped += row.probes_skipped;
        }
        if row.kind != "probe" && row.kept_warm_requests == 0 {
            continue;
        }
        // A row filed under no pool lists under its account, where the idle-age counters count that account's
        // windows, so the two sources meet on one row.
        let pool = if row.pool_id.is_empty() || row.pool_id == UNATTRIBUTED {
            &row.account_id
        } else {
            &row.pool_id
        };
        let entry = pools.entry(pool.clone()).or_default();
        entry.credited += row.kept_warm_requests;
        if row.kind == "probe" {
            entry.probes += row.succeeded;
        }
        entry.wasted += row.wasted_probes;
        for (reason, count) in &row.wasted_probe_reasons {
            *entry.wasted_reasons.entry(reason.clone()).or_default() += count;
        }
    }
    let returns = state.keepalive.idle_returns();
    for pool in returns.keys() {
        pools.entry(pool.clone()).or_default();
    }
    let none = IdleReturns::default();
    let pools: Vec<_> = pools
        .into_iter()
        .map(|(pool_id, day)| {
            let mut reasons: BTreeMap<String, u64> = Wasted::ALL
                .iter()
                .map(|w| (w.as_str().to_string(), 0))
                .collect();
            reasons.extend(day.wasted_reasons);
            let counters = returns.get(&pool_id).unwrap_or(&none);
            let idle_returns: Vec<_> = counters
                .buckets
                .iter()
                .enumerate()
                .map(|(i, bucket)| {
                    json!({
                        "from_seconds": IDLE_AGE_BOUNDS_SECONDS[i],
                        "reached": bucket.reached,
                        "returned": bucket.returned,
                        "to_seconds": IDLE_AGE_BOUNDS_SECONDS.get(i + 1),
                    })
                })
                .collect();
            json!({
                "credited_24h": day.credited,
                "idle_returns": idle_returns,
                "pool_id": pool_id,
                "probes_24h": day.probes,
                "wasted_24h": day.wasted,
                "wasted_reasons_24h": reasons,
            })
        })
        .collect();
    let stats = state.keepalive.stats();
    let limits = &state.settings.keepalive;
    Json(json!({
        "agents": state.keepalive.agent_counts(),
        "bus_gaps": stats.bus_gaps,
        "expired_between_turns_24h": expired,
        "limits": {
            "agents": { "count": stats.agents, "max": limits.max_agents },
            "retained_bytes": { "count": stats.retained_bytes, "max": limits.max_retained_bytes },
            "sessions": { "count": stats.live_sessions, "max": limits.max_sessions },
        },
        "pools": pools,
        "probes_24h": probes,
    }))
    .into_response()
}
