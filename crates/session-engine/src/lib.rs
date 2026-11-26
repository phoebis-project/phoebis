//! The cache keepalive engine: retains a replayable recipe of every eligible request and probes it before the
//! prompt cache expires, through the relay's own failover loop, so a session that went idle reads warm when it
//! comes back.

mod companion;
mod engine;
mod routes;
mod sessions;
mod warming;

pub use companion::companion_router;
pub use engine::config::{
    KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, LimitsError, PolicyError, FIVE_MINUTES_MS,
    LEASE_MS, LEASE_SECONDS, MAX_WORKERS, MIN_AFFINITY_SECONDS, ONE_HOUR_MS, PERSIST_INTERVAL,
};
pub use engine::view::{
    recipe_state, AgentCounts, CompanionState, KeepaliveSummary, Lifecycle, RecipeState,
    RecipeView, SessionView, SummaryState,
};
pub use engine::{
    continues_prefix, guard, idle_age_bucket, pool_fundings, spawn_observer, AgentState, Engine,
    EngineParts, EngineStats, Funding, Guard, IdleReturnBucket, IdleReturns, Park, ParkKind,
    PrefixAnchor, Reason, SavingsUnit, SessionKey, Wasted, IDLE_AGE_BOUNDS_SECONDS, MAIN_AGENT,
};
pub use routes::{events_router, EVENTS_PATH, MAX_BODY_BYTES};
pub use sessions::facts::{AgentFacts, SessionFacts, StoredSessionFacts};
pub use warming::breakeven::{
    break_even_ms, factors_for, resolve, BreakEvenFactors, CeilingInputs, Ceilings, Range,
    ResolvedCeilings,
};
pub use warming::probe::PROBE_DEADLINE;
pub use warming::recipe::{
    make_recipe, request_ttl_ms, valid_opaque, Recipe, MAX_HEADER_BYTES, MAX_HEADER_VALUE,
};
pub use warming::usage::{ProbeUsage, MAX_USAGE_BYTES};
