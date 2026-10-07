//! The keepalive views: every retained recipe, and the overview tiles the sessions page shows.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_keepalive::KeepalivePolicy;
use phoebis_ledger::{hour_of, read_rollups, RollupSelect};
use serde_json::json;

use crate::analytics::ledger_failure;
use crate::AdminState;

pub(crate) async fn recipes(State(state): State<Arc<AdminState>>) -> Response {
    Json(json!({ "recipes": state.keepalive.recipes() })).into_response()
}

pub(crate) async fn overview(State(state): State<Arc<AdminState>>) -> Response {
    let now = (state.clock)();
    let day = hour_of(now) - 24 * 3_600;
    let week = hour_of(now) - 7 * 24 * 3_600;
    let to = hour_of(now) + 3_600;
    let rows = match state
        .ledger
        .db()
        .read(move |c| read_rollups(c, week, to, &RollupSelect::default()))
        .await
    {
        Ok(rows) => rows,
        Err(e) => return ledger_failure(e),
    };
    let mut expired = 0u64;
    let mut net = 0.0;
    let mut probes = phoebis_ledger::ProbeCounts::default();
    for row in &rows {
        net += row.saved_usd_gross - row.probe_cost_usd;
        if row.hour >= day {
            expired += row
                .cache_miss_reasons
                .get("expired_between_turns")
                .copied()
                .unwrap_or(0);
            if row.kind == "probe" {
                probes.ok += row.succeeded;
                probes.failed +=
                    row.failed + row.rate_limited + row.exhausted + row.rejected + row.incomplete;
                probes.held += row.probes_held;
                probes.migrated += row.probe_migrated;
                probes.skipped += row.probes_skipped;
            }
        }
    }
    let spent: BTreeMap<String, f64> = state.keepalive.spent_today();
    let mut cost_today = Vec::new();
    if let Ok(pools) = state.store.pools() {
        for pool in pools {
            let policy = KeepalivePolicy::from_json(&pool.keepalive_json);
            if policy.enabled {
                cost_today.push(json!({ "cap_usd": policy.probe_cost_cap_usd_per_day, "pool_id": pool.id, "usd": spent.get(&pool.id).copied().unwrap_or(0.0) }));
            }
        }
    }
    Json(json!({
        "agents": state.keepalive.agent_counts(),
        "bus_gaps": state.keepalive.stats().bus_gaps,
        "cost_today": cost_today,
        "expired_between_turns_24h": expired,
        "net_saved_7d_usd": net,
        "probes_24h": probes,
    }))
    .into_response()
}
