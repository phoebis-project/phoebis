//! The session engine and the cache warming behind one handle. The session engine keeps the record of each session and
//! agent that the companion reports. The cache warming retains a replayable recipe of every eligible request and probes
//! it before the prompt cache expires, through the relay's own failover loop, so a session that went idle reads warm
//! when it comes back.

mod companion;
mod engine;
mod routes;
mod sessions;
mod warming;

pub use companion::companion_router;
pub use engine::config::PERSIST_INTERVAL;
pub use engine::view::{Lifecycle, RecipeView, SessionView};
pub use engine::{spawn_observer, Engine, EngineParts, EngineStats};
pub use routes::{events_router, EVENTS_PATH};
pub use sessions::config::{SessionCaps, LEASE_MS, LEASE_SECONDS};
pub use sessions::facts::{AgentFacts, SessionFacts, StoredSessionFacts};
pub use sessions::state::{AgentState, ParkKind, MAIN_AGENT};
pub use sessions::view::{CompanionState, SessionCounts};
pub use warming::breakeven::{
    break_even_ms, factors_for, pool_fundings, resolve, BreakEvenFactors, CeilingInputs, Ceilings,
    Funding, Range, ResolvedCeilings,
};
pub use warming::causes::{Reason, Wasted};
pub use warming::config::{
    KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, LimitsError, PolicyError, FIVE_MINUTES_MS,
    MAX_WORKERS, ONE_HOUR_MS,
};
pub use warming::guard::{guard, Guard};
pub use warming::idle::{
    continues_prefix, idle_age_bucket, IdleReturnBucket, IdleReturns, PrefixAnchor,
    IDLE_AGE_BOUNDS_SECONDS,
};
pub use warming::probe::PROBE_DEADLINE;
pub use warming::recipe::{make_recipe, request_ttl_ms, Recipe};
pub use warming::savings::SavingsUnit;
pub use warming::usage::{ProbeUsage, MAX_USAGE_BYTES};
pub use warming::view::{AgentCounts, KeepaliveSummary, RecipeState, WarmingCounts};
