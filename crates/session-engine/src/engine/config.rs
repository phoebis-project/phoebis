//! The process-wide limits the bootstrap file carries, read once at start, and the interval of the engine's save.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::warming::config::bound;

/// How often the engine writes its changed sessions, agents and counters to the registry. The shutdown writes
/// whatever is left.
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct KeepaliveLimits {
    pub max_agents: usize,
    pub max_retained_bytes: usize,
    pub max_sessions: usize,
}

impl Default for KeepaliveLimits {
    fn default() -> Self {
        KeepaliveLimits {
            max_agents: 1_024,
            max_retained_bytes: 256 << 20,
            max_sessions: 128,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LimitsError {
    #[error("keepalive.{field} must be between {min} and {max}, not {value}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        value: u64,
    },
}

impl KeepaliveLimits {
    /// `max_request_bytes` is the relay's per-body bound, which is also the largest recipe the engine retains; a
    /// retained budget below it could not hold one recipe the relay accepted.
    pub fn validate(&self, max_request_bytes: usize) -> Result<(), LimitsError> {
        let range = |field, min, max, value| LimitsError::OutOfRange {
            field,
            min,
            max,
            value,
        };
        bound("max_agents", self.max_agents as u64, 1, 16_384, range)?;
        bound("max_sessions", self.max_sessions as u64, 1, 4_096, range)?;
        bound(
            "max_retained_bytes",
            self.max_retained_bytes as u64,
            max_request_bytes as u64,
            512 << 20,
            range,
        )?;
        Ok(())
    }
}
