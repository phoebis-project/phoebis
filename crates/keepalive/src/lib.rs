//! The cache keepalive engine: retains a replayable recipe of every eligible request and probes it before the
//! prompt cache expires, through the relay's own failover loop, so a session that went idle reads warm when it
//! comes back.

mod config;

pub use config::{
    KeepaliveLimits, KeepalivePolicy, LimitsError, PolicyError, FIVE_MINUTES_MS,
    MIN_AFFINITY_SECONDS, ONE_HOUR_MS,
};
