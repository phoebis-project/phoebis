//! The keepalive views: every retained recipe, and the overview tiles the sessions page shows.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_attention::{KeepaliveCap, KeepaliveFacts, KeepaliveStanding};
use phoebis_keepalive::{
    Engine, EngineStats, IdleReturns, KeepaliveLimits, Wasted, IDLE_AGE_BOUNDS_SECONDS,
};
use phoebis_ledger::{read_overview_rollups, trailing_day_hours, UNATTRIBUTED};
use utoipa::ToSchema;

use crate::analytics::ledger_failure;
use crate::openapi::{ErrorBody, LedgerFailure};
use crate::routes::{failure, AdminQuery};
use crate::AdminState;

/// The overview tiles the sessions page shows: the agents, the engine's limits, one row per pool with keepalive
/// rows in the last 24 hours or idle-age counters (an account outside every pool lists under its own id), and the
/// probes of the last 24 hours.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct KeepaliveOverview {
    /// The agents holding a recipe, by the mod's lifecycle word `busy`, `idle` or `parked`.
    #[schema(value_type = crate::openapi::AgentCountsView)]
    pub agents: phoebis_keepalive::AgentCounts,
    /// The times the keepalive engine found it had missed bus events.
    pub bus_gaps: u64,
    pub expired_between_turns_24h: u64,
    /// The last gap's instant, null before the first gap.
    pub last_bus_gap_at: Option<i64>,
    /// `agents`, `retained_bytes` and `sessions`, each a `count` of what its cap bounds, where a session holds a slot
    /// from its first request until its end, and an agent's record from that request until it leaves, beside the
    /// `max` the bootstrap file's `keepalive` block sets.
    pub limits: OverviewLimits,
    pub pools: Vec<PoolKeepalive>,
    /// `ok`, `plan_ok`, `failed`, `held`, `skipped`, where `plan_ok` counts the plan-funded probes of `ok` and `held`
    /// the probes the router refused with `503 ledger_behind` while its ledger caught up, outside `failed`.
    #[schema(value_type = crate::openapi::ProbeCountsView)]
    pub probes_24h: phoebis_ledger::ProbeCounts,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct OverviewLimits {
    pub agents: Limit,
    pub retained_bytes: Limit,
    pub sessions: Limit,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct Limit {
    pub count: usize,
    pub max: usize,
}

/// One pool's keepalive day.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct PoolKeepalive {
    /// The requests a probe kept warm.
    pub credited_24h: u64,
    /// The ten buckets of the return rate by idle age (the keepalive page's section of that name), summed over the
    /// UTC days of `range`, each `from_seconds`, `reached`, `returned` and `to_seconds`.
    pub idle_returns: Vec<IdleReturn>,
    pub pool_id: String,
    /// The probes the upstream served.
    pub probes_24h: u64,
    pub wasted_24h: u64,
    /// Every wasted reason word, zero or not.
    pub wasted_reasons_24h: BTreeMap<String, u64>,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct IdleReturn {
    pub from_seconds: u64,
    pub reached: u64,
    pub returned: u64,
    /// Null on the last bucket.
    pub to_seconds: Option<u64>,
}

/// The engine as the attention reader sees it: the counters and the caps the overview answers, and the pools whose
/// policy warms caches.
pub struct EngineFacts {
    pub engine: Arc<Engine>,
}

impl KeepaliveFacts for EngineFacts {
    fn standing(&self) -> KeepaliveStanding {
        let stats = self.engine.stats();
        KeepaliveStanding {
            bus_gaps: stats.bus_gaps,
            last_bus_gap_at: stats.last_bus_gap_at,
            caps: caps_of(&stats, self.engine.limits()).into(),
            warming: self.engine.warming(),
        }
    }
}

/// The three caps, which the overview's `limits` and the reader's standing both read, so the two never count a cap
/// differently.
fn caps_of(stats: &EngineStats, limits: &KeepaliveLimits) -> [KeepaliveCap; 3] {
    [
        KeepaliveCap {
            name: "sessions",
            count: stats.live_sessions,
            max: limits.max_sessions,
        },
        KeepaliveCap {
            name: "agents",
            count: stats.agents,
            max: limits.max_agents,
        },
        KeepaliveCap {
            name: "retained",
            count: stats.retained_bytes,
            max: limits.max_retained_bytes,
        },
    ]
}

/// `recipes`, one row per agent that has sent a real request.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct Recipes {
    #[schema(value_type = Vec<crate::openapi::RecipeViewSchema>)]
    pub recipes: Vec<phoebis_keepalive::RecipeView>,
}

/// List the keepalive recipes.
#[utoipa::path(
    get,
    path = "/admin/keepalive/recipes",
    operation_id = "keepalive_recipes",
    tag = "Keepalive",
    responses(
        (status = 200, description = "The recipes.", body = Recipes),
    ),
)]
pub(crate) async fn recipes(State(state): State<Arc<AdminState>>) -> Response {
    Json(Recipes {
        recipes: state.keepalive.recipes(),
    })
    .into_response()
}

/// `GET /admin/keepalive/overview`'s query.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct OverviewQuery {
    /// `7d` (the default) or `30d`: the UTC days the idle-return counters sum over, the current day and the 6 or 29
    /// before it. Another word answers 400.
    pub range: Option<String>,
}

#[derive(Default)]
struct PoolDay {
    credited: u64,
    probes: u64,
    wasted: u64,
    wasted_reasons: BTreeMap<String, u64>,
}

/// Read the keepalive overview.
#[utoipa::path(
    get,
    path = "/admin/keepalive/overview",
    operation_id = "keepalive_overview",
    tag = "Keepalive",
    params(OverviewQuery),
    responses(
        (status = 200, description = "The overview.", body = KeepaliveOverview),
        (status = 400, description = "`range is 7d or 30d`", body = ErrorBody, example = json!({"error": "range is 7d or 30d"})),
        (status = 500, response = LedgerFailure),
    ),
)]
pub(crate) async fn overview(
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<OverviewQuery>,
) -> Response {
    let days = match q.range.as_deref().unwrap_or("7d") {
        "7d" => 7,
        "30d" => 30,
        _ => return failure(StatusCode::BAD_REQUEST, "range is 7d or 30d"),
    };
    let now = (state.clock)();
    let (day, to) = trailing_day_hours(now);
    let (expired, rows) = match state
        .ledger
        .db()
        .read(move |c| read_overview_rollups(c, day, to))
        .await
    {
        Ok(read) => read,
        Err(e) => return ledger_failure(e),
    };
    let mut probes = phoebis_ledger::ProbeCounts::default();
    let mut pools: BTreeMap<String, PoolDay> = BTreeMap::new();
    for row in &rows {
        let probe = row.kind == phoebis_bus::Kind::Probe.as_str();
        if probe {
            probes.add_row(row.probe_view());
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
        if probe {
            entry.probes += row.succeeded;
        }
        entry.wasted += row.wasted_probes;
        for (reason, count) in &row.wasted_probe_reasons {
            *entry.wasted_reasons.entry(reason.clone()).or_default() += count;
        }
    }
    let returns = state.keepalive.idle_returns_over(now, days);
    for pool in returns.keys() {
        pools.entry(pool.clone()).or_default();
    }
    let none = IdleReturns::default();
    let pools: Vec<PoolKeepalive> = pools
        .into_iter()
        .map(|(pool_id, day)| {
            let mut reasons: BTreeMap<String, u64> = Wasted::ALL
                .iter()
                .map(|w| (w.as_str().to_string(), 0))
                .collect();
            reasons.extend(day.wasted_reasons);
            let counters = returns.get(&pool_id).unwrap_or(&none);
            let idle_returns = counters
                .buckets
                .iter()
                .enumerate()
                .map(|(i, bucket)| IdleReturn {
                    from_seconds: IDLE_AGE_BOUNDS_SECONDS[i],
                    reached: bucket.reached,
                    returned: bucket.returned,
                    to_seconds: IDLE_AGE_BOUNDS_SECONDS.get(i + 1).copied(),
                })
                .collect();
            PoolKeepalive {
                credited_24h: day.credited,
                idle_returns,
                pool_id,
                probes_24h: day.probes,
                wasted_24h: day.wasted,
                wasted_reasons_24h: reasons,
            }
        })
        .collect();
    let stats = state.keepalive.stats();
    let [sessions, agents, retained] = caps_of(&stats, state.keepalive.limits()).map(|c| Limit {
        count: c.count,
        max: c.max,
    });
    Json(KeepaliveOverview {
        agents: state.keepalive.agent_counts(),
        bus_gaps: stats.bus_gaps,
        expired_between_turns_24h: expired,
        last_bus_gap_at: stats.last_bus_gap_at,
        limits: OverviewLimits {
            agents,
            retained_bytes: retained,
            sessions,
        },
        pools,
        probes_24h: probes,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use crate::openapi::round_trip::{round_trips, round_trips_each};
    use crate::openapi::{AgentCountsView, ProbeCountsView, RecipeViewSchema};

    /// The overview with its two mirrored fields typed as the mirrors, so the whole body round trips.
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OverviewTwin {
        agents: AgentCountsView,
        bus_gaps: u64,
        expired_between_turns_24h: u64,
        last_bus_gap_at: Option<i64>,
        limits: super::OverviewLimits,
        pools: Vec<super::PoolKeepalive>,
        probes_24h: ProbeCountsView,
    }

    #[test]
    fn the_keepalive_fixtures_round_trip_through_their_declared_types_and_mirrors() {
        round_trips::<OverviewTwin>("keepalive_overview");
        round_trips_each::<RecipeViewSchema>("keepalive_recipes", &["recipes", "*"]);
    }
}
