//! The session engine's lease and its two caps.

/// How long the companion's lease over a session lasts before the pool's idle limits take over.
pub const LEASE_SECONDS: u64 = 90;
pub const LEASE_MS: i64 = LEASE_SECONDS as i64 * 1_000;

/// The two caps of the session engine, which the bootstrap file's `sessions` block sets.
#[derive(Clone, Debug)]
pub struct SessionCaps {
    pub max_agents: usize,
    pub max_sessions: usize,
}

impl Default for SessionCaps {
    fn default() -> Self {
        SessionCaps {
            max_agents: 1_024,
            max_sessions: 128,
        }
    }
}
