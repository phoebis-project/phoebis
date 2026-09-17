//! The cache keepalive engine: retains a replayable recipe of every eligible request and probes it before the
//! prompt cache expires, through the relay's own failover loop, so a session that went idle reads warm when it
//! comes back.

mod config;
mod engine;
mod events;
mod probe;
mod recipe;
mod routes;
mod savings;
mod usage;

pub use config::{
    KeepaliveLimits, KeepalivePolicy, LimitsError, PolicyError, FIVE_MINUTES_MS,
    MIN_AFFINITY_SECONDS, ONE_HOUR_MS,
};
pub use engine::{
    spawn_observer, AgentState, AgentStatus, Engine, EngineParts, EngineStats, Reason, SessionKey,
    Wasted, MAIN_AGENT,
};
pub use events::{
    decode_query_event, rfc3339, Ack, Event, EventError, Snapshot, SnapshotAgent, SnapshotSession,
    EVENT_FIELDS,
};
pub use probe::{ProbeOutcome, ProbeRecord, PROBE_DEADLINE};
pub use recipe::{
    make_recipe, request_ttl_ms, valid_id, valid_opaque, Recipe, MAX_HEADER_BYTES,
    MAX_HEADER_VALUE, RECIPE_HEADERS,
};
pub use routes::{companion_router, EVENTS_PATH, SNAPSHOT_PATH};
pub use usage::{ProbeUsage, MAX_USAGE_BYTES};

/// What a probe declares to the relay when the engine sends it: which key it runs under, and whether the
/// operator has verified that oauth's expiry-crossing behavior is safe to probe.
pub fn probe_marker(engine: &Engine, key_id: &str) -> phoebis_relay::Probe {
    phoebis_relay::Probe {
        key_id: key_id.into(),
        allow_oauth: engine.limits().oauth_prewarm_verified,
    }
}
