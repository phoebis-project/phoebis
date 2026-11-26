//! The session engine's saved rows: a session's row, and a row of each of its agents beside the warming's row of that
//! agent. Each payload carries a record version of its own.

use std::collections::BTreeMap;

use phoebis_store::{CompanionAgentRow, CompanionSessionRow};
use serde::{Deserialize, Serialize};

use crate::sessions::config::LEASE_MS;
use crate::sessions::state::{Agent, AgentState, HeldPark, Park, ParkKind, Session};

/// The session payload's shape. A session row of an older version is dropped at the load with its agents' rows: at
/// version 1 their payloads carried no funding and at version 2 a dollar in place of the probe's usage, tier, instant
/// and band, so none settles under this build's rule.
pub(crate) const SESSION_RECORD_VERSION: u32 = 3;

/// The shape of the session engine's agent payload. A row of another version is dropped at the load, and the agent's
/// row of the warming with it.
pub(crate) const AGENT_RECORD_VERSION: u32 = 3;

/// A session row's key in the store: key id, key generation, session id.
pub(crate) type PersistKey = (String, i64, String);

/// Every field of `Session` but its agents, which have rows of their own, and the restored standing, which the load
/// derives from `attended`.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedSession {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub instance: String,
    pub sequence: u64,
    pub lease: i64,
    pub attended: bool,
    pub ended: bool,
    pub last_report_at: i64,
    pub last_real_at: Option<i64>,
    /// Absent from a row an earlier build saved, which loads as `None` and takes its slot from `last_real_at`: that
    /// build wrote the stamp only at a request that took the slots.
    #[serde(default)]
    pub slotted: Option<bool>,
    pub cache_warming: bool,
}

/// An agent's saved members of the session engine: its identity, its lifecycle with the held park in four members,
/// and its request generation. The warming's members of the same agent ride the warming's row.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedAgent {
    pub id: String,
    pub key_id: String,
    pub incarnation: u64,
    pub state: AgentState,
    pub deadline: Option<i64>,
    pub park_kind: Option<ParkKind>,
    pub wake_at: Option<i64>,
    pub park_began: Option<i64>,
    pub park_ended_at: Option<i64>,
    pub generation: u64,
    /// When the released record leaves the engine, as `Agent.leaves_at` carries it, `None` on a record that is not
    /// released.
    pub leaves_at: Option<i64>,
    pub last_activity: i64,
    pub last_presence_at: Option<i64>,
}

impl PersistedSession {
    fn of(key: &PersistKey, session: &Session) -> PersistedSession {
        PersistedSession {
            key_id: key.0.clone(),
            key_generation: session.key_generation,
            session_id: key.2.clone(),
            instance: session.instance.clone(),
            sequence: session.sequence,
            lease: session.lease,
            attended: session.attended,
            ended: session.ended,
            last_report_at: session.last_report_at,
            last_real_at: session.last_real_at,
            slotted: Some(session.slotted),
            cache_warming: session.cache_warming,
        }
    }

    /// Silent until the mod's next heartbeat or registration re-adopts it: the lease at zero, so the next report
    /// answers `instance_ended` and the same instance registers again, and the sequence kept, so that registration is
    /// the one the mod would send next. A session saved attended keeps the attended reading of the ceilings for one
    /// lease length from `now`, so the ticks before that registration discard no park as idle.
    pub(crate) fn restore(self, now: i64) -> Session {
        Session {
            key_generation: self.key_generation,
            instance: self.instance,
            sequence: self.sequence,
            lease: 0,
            attended: false,
            ended: false,
            last_report_at: self.last_report_at,
            last_real_at: self.last_real_at,
            slotted: self.slotted.unwrap_or(self.last_real_at.is_some()),
            restored_until: self.attended.then_some(now + LEASE_MS),
            instance_mismatches: 0,
            agents: BTreeMap::new(),
            cache_warming: self.cache_warming,
        }
    }
}

impl PersistedAgent {
    fn of(agent: &Agent) -> PersistedAgent {
        let (deadline, park_kind, wake_at) = Park::parts(agent.park.map(|held| held.park));
        PersistedAgent {
            id: agent.id.clone(),
            key_id: agent.key_id.clone(),
            incarnation: agent.incarnation,
            state: agent.state,
            deadline,
            park_kind,
            wake_at,
            park_began: agent.park.map(|held| held.began),
            park_ended_at: agent.park_ended_at,
            generation: agent.generation,
            leaves_at: agent.leaves_at,
            last_activity: agent.last_activity,
            last_presence_at: agent.last_presence_at,
        }
    }

    /// The agent the payload describes.
    fn restore(self) -> Agent {
        let mut agent = Agent::new(&self.id, &self.key_id, self.incarnation, self.last_activity);
        agent.state = self.state;
        agent.park = self
            .park_kind
            .zip(self.park_began)
            .map(|(kind, began)| HeldPark {
                began,
                // A row that an earlier build saved can hold a part that the parser now refuses.
                park: Park::new(kind, self.deadline, self.wake_at),
            });
        agent.park_ended_at = self.park_ended_at;
        agent.generation = self.generation;
        agent.leaves_at = self.leaves_at;
        agent.last_presence_at = self.last_presence_at;
        agent
    }
}

/// One session row as the store keeps it.
pub(crate) fn session_row(
    key: &PersistKey,
    session: &Session,
    now: i64,
) -> Result<CompanionSessionRow, serde_json::Error> {
    Ok(CompanionSessionRow {
        key_id: key.0.clone(),
        key_generation: key.1,
        session_id: key.2.clone(),
        record_version: SESSION_RECORD_VERSION,
        payload: serde_json::to_string(&PersistedSession::of(key, session))?,
        updated_at: now,
        seen_at: Some(session.last_report_at),
    })
}

/// One agent row of the session engine as the store keeps it.
pub(crate) fn agent_row(
    key: &PersistKey,
    agent: &Agent,
    now: i64,
) -> Result<CompanionAgentRow, serde_json::Error> {
    Ok(CompanionAgentRow {
        key_id: key.0.clone(),
        key_generation: key.1,
        session_id: key.2.clone(),
        agent_id: agent.id.clone(),
        record_version: AGENT_RECORD_VERSION,
        payload: serde_json::to_string(&PersistedAgent::of(agent))?,
        updated_at: now,
    })
}

/// The agent a row describes, `None` when its version is one this build does not read or its payload does not parse.
pub(crate) fn decode_agent(row: &CompanionAgentRow) -> Option<Agent> {
    (row.record_version == AGENT_RECORD_VERSION)
        .then(|| serde_json::from_str::<PersistedAgent>(&row.payload).ok())
        .flatten()
        .map(PersistedAgent::restore)
}
