//! What the session engine answers the reads that join both components: a session's companion word, its presence and
//! the session engine's counts.

use crate::sessions::state::{Session, SessionsState, MAIN_AGENT};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionState {
    None,
    Reporting,
    Silent,
}

impl Session {
    pub(crate) fn companion(&self) -> CompanionState {
        // An ended session keeps its tombstone for the lifecycle protocol, but its companion is
        // gone: the engine refuses further events under that instance, and the lease sweep leaves an
        // ended session alone, so `attended` stays as the last heartbeat left it.
        if self.ended {
            CompanionState::None
        } else if self.attended {
            CompanionState::Reporting
        } else {
            CompanionState::Silent
        }
    }

    pub(crate) fn companion_since(&self) -> Option<i64> {
        // The end and the lapse are both the moment the companion stopped reporting, which is
        // the last report either way; a reporting companion has no such moment.
        (self.companion() != CompanionState::Reporting).then_some(self.last_report_at)
    }

    pub(crate) fn presence_age_seconds(&self, now: i64) -> Option<i64> {
        self.agents
            .get(MAIN_AGENT)
            .and_then(|a| a.last_presence_at)
            .map(|at| (now - at).max(0) / 1_000)
    }
}

/// The session engine's counts.
#[derive(Debug)]
pub struct SessionCounts {
    pub agents: usize,
    /// Every session entry the engine holds: a session that holds no slot counts here and not in `live`.
    pub entries: usize,
    /// The sessions `max_sessions` bounds: a registered session that has sent no real request holds no slot, and an
    /// ended session keeps its entry until the expiry and holds none.
    pub live: usize,
}

impl SessionsState {
    pub(crate) fn counts(&self) -> SessionCounts {
        SessionCounts {
            agents: self.agent_count(),
            entries: self.entries.len(),
            live: self.live_sessions(),
        }
    }
}
