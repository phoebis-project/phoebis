//! The cache keepalive engine: retains a replayable recipe of every eligible request and probes it before the
//! prompt cache expires, through the relay's own failover loop, so a session that went idle reads warm when it
//! comes back.

mod config;
mod recipe;
mod usage;

pub use config::{
    KeepaliveLimits, KeepalivePolicy, LimitsError, PolicyError, FIVE_MINUTES_MS,
    MIN_AFFINITY_SECONDS, ONE_HOUR_MS,
};
pub use recipe::{
    make_recipe, request_ttl_ms, valid_id, valid_opaque, Recipe, MAX_HEADER_BYTES,
    MAX_HEADER_VALUE, RECIPE_HEADERS,
};
pub use usage::{ProbeUsage, MAX_USAGE_BYTES};
