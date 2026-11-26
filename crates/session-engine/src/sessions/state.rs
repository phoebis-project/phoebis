//! The session engine's part of the engine's locked state.

use std::collections::BTreeMap;

use crate::engine::{Session, SessionKey};

/// The session engine's state under the engine's lock: each session's record with its agents' records, and the bounds
/// that a registration, an arrival and the sweep read.
pub(crate) struct SessionsState {
    pub entries: BTreeMap<SessionKey, Session>,
    pub max_agents: usize,
    pub max_sessions: usize,
    pub next_incarnation: u64,
    pub session_expiry_ms: i64,
}

impl SessionsState {
    pub(crate) fn new(
        max_agents: usize,
        max_sessions: usize,
        session_expiry_ms: i64,
    ) -> SessionsState {
        SessionsState {
            entries: BTreeMap::new(),
            max_agents,
            max_sessions,
            next_incarnation: 0,
            session_expiry_ms,
        }
    }
}
