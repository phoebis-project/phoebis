//! The cache keepalive engine: retains a replayable recipe of every eligible request and probes it before the
//! prompt cache expires, through the relay's own failover loop, so a session that went idle reads warm when it
//! comes back.

mod breakeven;
mod config;
mod engine;
mod events;
mod persist;
mod probe;
mod recipe;
mod routes;
mod savings;
mod usage;
mod view;

pub use breakeven::{
    break_even_ms, factors_for, resolve, BreakEvenFactors, CeilingInputs, Ceilings, Range,
    ResolvedCeilings,
};
pub use config::{
    KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, LimitsError, PolicyError, FIVE_MINUTES_MS,
    LEASE_MS, LEASE_SECONDS, MAX_WORKERS, MIN_AFFINITY_SECONDS, ONE_HOUR_MS, PERSIST_INTERVAL,
};
pub use engine::{
    continues_prefix, guard, idle_age_bucket, pool_fundings, spawn_observer, AgentState,
    AgentStatus, Engine, EngineParts, EngineStats, Funding, Guard, IdleReturnBucket, IdleReturns,
    ParkKind, PrefixAnchor, Reason, SavingsUnit, SessionKey, Wasted, IDLE_AGE_BOUNDS_SECONDS,
    MAIN_AGENT,
};
pub use events::{
    decode_json_event, Ack, AckAgent, Event, EventError, EventReport, Reported, PROTOCOL,
};
pub use probe::{ProbeOutcome, ProbeRecord, PROBE_DEADLINE};
pub use recipe::{
    make_recipe, request_ttl_ms, valid_id, valid_opaque, Recipe, MAX_HEADER_BYTES, MAX_HEADER_VALUE,
};
pub use routes::{keepalive_router, EVENTS_PATH, MAX_BODY_BYTES};
pub use usage::{ProbeUsage, MAX_USAGE_BYTES};
pub use view::{
    recipe_state, AgentCounts, CompanionState, KeepaliveSummary, RecipeState, RecipeView,
    SessionView,
};

/// What a probe declares to the relay when the engine sends it: the key it runs under and the key's generation,
/// so the relay refuses it once the key id names a replacement.
pub fn probe_marker(key_id: &str, key_generation: i64) -> phoebis_relay::Probe {
    phoebis_relay::Probe {
        key_id: key_id.into(),
        key_generation,
    }
}
