//! The engine's state across a restart: the session engine's sessions and agents, the warming's record of each agent
//! with its retained prompt, and the idle-age counters, written behind the map to the registry and read back once at
//! start. Each component builds and restores its own rows, and the engine above holds the dirty set, the one save of
//! both components' rows and the order of the load. The map stays the truth while the engine runs, and the store is
//! what the next process starts from. The relay's session memory has the same shape: a dirty set under revisions, one
//! flush at a time on the blocking pool, and an acknowledgment that retires only what the save carried. The facts
//! that a companion reports go to the registry the same way, each set as the report filed it, and the save that a
//! filing asks for runs at once.

use std::collections::BTreeMap;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use phoebis_routing::RegistrySnapshot;
use phoebis_store::engine_state::BodyWrite;
use phoebis_store::{
    CompanionAgentRow, EngineBatch, EngineState, FactsKey, FactsKind, FactsRow, KeepaliveAgentRow,
};
use serde::Deserialize;

use crate::engine::{Engine, Inner};
use crate::sessions::facts::{
    AgentFacts, RawAgentFacts, RawSessionFacts, SessionFacts, StoredSessionFacts,
};
use crate::sessions::persist::{decode_session, session_row, PersistKey, SESSION_RECORD_VERSION};
use crate::sessions::state::{
    agent_ref, key_stands, Agent, Session, SessionKey, AGENT_HELD, MAIN_AGENT,
};
use crate::warming::agent::{warming_mut, warming_ref, AgentWarming};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::KeepaliveTiming;
use crate::warming::observation::{Observation, Settlement};
use crate::warming::persist::{decode_warming, Disposed, PersistedObservation};
use crate::warming::state::{Change, Changes};

/// The shape of a stored set's value: a JSON object of the members of the row's kind under the raw form's names, an
/// absent one as `null`.
pub(crate) const FACTS_RECORD_VERSION: u32 = 1;

/// One filed set: the value to write, or `None` where the row leaves, with the instant of the filing's report.
#[derive(Debug)]
pub(crate) struct FiledFacts {
    pub value: Option<Vec<u8>>,
    pub seen_at: i64,
    pub revision: u64,
}

/// The rows changed since their last acknowledged save, each under the revision of its latest change, and the rows
/// removed since, under the revision of the removal. Every key carries the key generation beside the store's
/// primary key, since `start` replaces a previous holder's session under a new generation in place and the batch
/// must name the old generation's row to delete it. A row sits in the changed half or the removed half, never both:
/// the later call wins.
#[derive(Debug, Default)]
pub(crate) struct Dirty {
    pub sessions: BTreeMap<PersistKey, u64>,
    pub agents: BTreeMap<(PersistKey, String), u64>,
    pub removed_sessions: BTreeMap<PersistKey, u64>,
    pub removed_agents: BTreeMap<(PersistKey, String), u64>,
    pub pools: BTreeMap<String, u64>,
    /// Each set of reported facts as its report filed it. The save writes the filed set and never reads one from the
    /// records, since a registration by another instance takes an agent's record out of the map while the set it
    /// reported still waits for its row.
    pub facts: BTreeMap<FactsKey, FiledFacts>,
    /// The leave of each stored set of one kind under a session, filed by a registration whose list leaves the kind
    /// out. Filing it takes back each set of the kind that is filed and not yet saved, and a set filed behind it
    /// stands, since the save runs the leaves of a kind ahead of its writes.
    pub fact_kinds: BTreeMap<(PersistKey, FactsKind), u64>,
}

/// What one flush carries, with the revision each row was read at, so the acknowledgment retires only those.
#[derive(Default)]
pub(crate) struct Snapshot {
    pub batch: EngineBatch,
    /// The settlements the load held for a save, taken under the lock this snapshot was read under: a save that lands
    /// releases them, and one that fails puts them back in front of whatever the load queued since.
    pub settled: Vec<Settlement>,
    sessions: Vec<(PersistKey, u64)>,
    agents: Vec<((PersistKey, String), u64)>,
    /// The agent rows whose body this save seals (true) or clears (false), at the revision each was read at.
    bodies: Vec<((PersistKey, String), u64, bool)>,
    /// The body each sealing row of `batch.warming_agents` carries, by the row's index, copied into its placeholder
    /// once the engine's lock is released.
    seals: Vec<(usize, Bytes)>,
    removed_sessions: Vec<(PersistKey, u64)>,
    removed_agents: Vec<((PersistKey, String), u64)>,
    pools: Vec<(String, u64)>,
    /// The engine's recorded cutoff as this snapshot read it, content of its own: a roll that dropped a day while no
    /// pool was dirty still has the registry's rows of that day to delete.
    purge_counters_before: Option<i64>,
    facts: Vec<(FactsKey, u64)>,
    fact_kinds: Vec<((PersistKey, FactsKind), u64)>,
}

impl Snapshot {
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.agents.is_empty()
            && self.removed_sessions.is_empty()
            && self.removed_agents.is_empty()
            && self.pools.is_empty()
            && self.purge_counters_before.is_none()
            && self.facts.is_empty()
            && self.fact_kinds.is_empty()
    }
}

/// An agent read back from its two rows: the session engine's record, the warming's, and the observation's fields
/// with its client label when the warming's row carried a recipe.
type Decoded = (Agent, AgentWarming, Option<PersistedObservation>);

/// An agent's two rows as the store answered them, either one absent.
#[derive(Default)]
struct AgentRows {
    agent: Option<CompanionAgentRow>,
    warming: Option<KeepaliveAgentRow>,
}

/// The agent its two rows describe, `None` unless both read under their record versions, so the load never restores a
/// record of one component without the other's.
fn decode_agent(
    agent_row: &CompanionAgentRow,
    warming_row: &KeepaliveAgentRow,
    timing: KeepaliveTiming,
) -> Option<Decoded> {
    let agent = crate::sessions::persist::decode_agent(agent_row)?;
    let (warming, observation) = decode_warming(warming_row, timing)?;
    Some((agent, warming, observation))
}

fn persist_key_of(key: &SessionKey, session: &Session) -> PersistKey {
    (key.0.clone(), session.key_generation, key.1.clone())
}

impl Inner {
    fn bump(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }

    fn persist_key(&self, key: &SessionKey) -> Option<PersistKey> {
        self.sessions
            .entries
            .get(key)
            .map(|session| persist_key_of(key, session))
    }

    fn session_of(&self, key: &PersistKey) -> Option<&Session> {
        self.sessions
            .held_by(&(key.0.clone(), key.2.clone()), key.1)
    }

    /// The session's row is dirty under the generation it is held under now.
    pub(crate) fn touch_session(&mut self, key: &SessionKey) {
        let Some(row) = self.persist_key(key) else {
            return;
        };
        let revision = self.bump();
        self.dirty.removed_sessions.remove(&row);
        self.dirty.sessions.insert(row, revision);
    }

    /// The agent's rows are dirty under its session's generation now.
    pub(crate) fn touch_agent(&mut self, key: &SessionKey, agent_id: &str) {
        let held = agent_ref(&self.sessions.entries, key, agent_id).is_some();
        let Some(row) = self.persist_key(key).filter(|_| held) else {
            return;
        };
        let revision = self.bump();
        let row = (row, agent_id.to_string());
        self.dirty.removed_agents.remove(&row);
        self.dirty.agents.insert(row, revision);
    }

    pub(crate) fn touch_pool(&mut self, pool_id: &str) {
        let revision = self.bump();
        self.dirty.pools.insert(pool_id.to_string(), revision);
    }

    /// The agent's rows go at the next save, under the generation its session is held under: called while the agent
    /// is still in the map.
    pub(crate) fn forget_agent(&mut self, key: &SessionKey, agent_id: &str) {
        let Some(row) = self.persist_key(key) else {
            return;
        };
        self.file_removed_agent(row, agent_id.to_string());
    }

    /// The session's row and every agent row under it go at the next save, filed under the generation the rows were
    /// saved under: called before the session leaves the map or a start replaces it.
    pub(crate) fn forget_session(&mut self, key: &SessionKey) {
        let Some(row) = self.persist_key(key) else {
            return;
        };
        let agents: Vec<String> = self.sessions.entries[key].agents.keys().cloned().collect();
        for agent_id in agents {
            self.forget_agent(key, &agent_id);
        }
        self.file_removed_session(row);
    }

    fn file_removed_session(&mut self, row: PersistKey) {
        let revision = self.bump();
        self.dirty.sessions.remove(&row);
        self.dirty.removed_sessions.insert(row, revision);
    }

    fn file_removed_agent(&mut self, row: PersistKey, agent_id: String) {
        let revision = self.bump();
        let row = (row, agent_id);
        self.dirty.agents.remove(&row);
        self.dirty.removed_agents.insert(row, revision);
    }

    /// Files one set of reported facts for the next save under the session's row key, `None` filing the row's leave.
    /// A later filing under the same key replaces the earlier one, so the save writes the last set applied under the
    /// lock.
    pub(crate) fn file_facts(
        &mut self,
        row: &PersistKey,
        kind: FactsKind,
        agent_id: &str,
        value: Option<Vec<u8>>,
        seen_at: i64,
    ) {
        let revision = self.bump();
        self.dirty.facts.insert(
            FactsKey {
                key_id: row.0.clone(),
                key_generation: row.1,
                session_id: row.2.clone(),
                kind,
                agent_id: agent_id.to_string(),
            },
            FiledFacts {
                value,
                seen_at,
                revision,
            },
        );
    }

    /// Files the leave of each stored set of `kind` under the session, and takes back each set of that kind and
    /// session that is filed and not yet saved. A set filed behind it stands.
    pub(crate) fn file_kind_leave(&mut self, row: &PersistKey, kind: FactsKind) {
        let revision = self.bump();
        self.dirty.facts.retain(|key, _| {
            !(key.key_id == row.0
                && key.key_generation == row.1
                && key.session_id == row.2
                && key.kind == kind)
        });
        self.dirty.fact_kinds.insert((row.clone(), kind), revision);
    }

    /// Drops each filed set whose key the registry no longer holds under the generation that filed it. The key's
    /// delete took the stored rows and the save writes no set of such a generation, so the entry would wait for the
    /// next landed save for nothing.
    pub(crate) fn forget_facts_of_dead_keys(&mut self, snapshot: &RegistrySnapshot) {
        self.dirty
            .facts
            .retain(|key, _| key_stands(snapshot, &key.key_id, key.key_generation));
    }

    /// The batch the dirty sets name, read as the map stands now, each row with the revision it was read at, and the
    /// settlements the load held for a save. No dirty entry is cleared here: `acknowledge` does that once the save has
    /// landed. A dirty row whose record left the map without a removal, which only `close` does, is carried by nothing
    /// and retired with the rest. A filed set of reported facts goes as its report filed it, whatever the map holds.
    pub(crate) fn persist_snapshot(&mut self, now: i64) -> Snapshot {
        let mut snapshot = Snapshot {
            settled: self.warming.take_settlements_after_save(),
            ..Snapshot::default()
        };
        for (row, revision) in &self.dirty.sessions {
            if let Some(session) = self.session_of(row) {
                match session_row(row, session, now) {
                    Ok(session_row) => {
                        snapshot.batch.sessions.push(session_row);
                        snapshot
                            .batch
                            .session_reports
                            .push((row.clone(), session.last_report_at));
                    }
                    Err(error) => {
                        tracing::warn!(key = %row.0, session = %row.2, %error, "a keepalive session could not be written")
                    }
                }
            }
            snapshot.sessions.push((row.clone(), *revision));
        }
        for ((row, agent_id), revision) in &self.dirty.agents {
            if let Some(agent) = self
                .session_of(row)
                .and_then(|session| session.agents.get(agent_id))
            {
                let warming = warming_ref(
                    &self.warming.agents,
                    &(row.0.clone(), row.2.clone()),
                    agent_id,
                )
                .expect(AGENT_HELD);
                // Both of the agent's rows or neither, so no batch writes one of them beside a stale other.
                match (
                    crate::sessions::persist::agent_row(row, agent, now),
                    crate::warming::persist::agent_row(row, agent, warming, now),
                ) {
                    (Ok(agent_row), Ok((warming_row, body))) => {
                        let sealed = match warming_row.body_write {
                            BodyWrite::Keep => None,
                            BodyWrite::Clear => Some(false),
                            BodyWrite::Seal(_) => Some(true),
                        };
                        if let Some(sealed) = sealed {
                            snapshot.bodies.push((
                                (row.clone(), agent_id.clone()),
                                *revision,
                                sealed,
                            ));
                        }
                        if let Some(body) = body {
                            snapshot
                                .seals
                                .push((snapshot.batch.warming_agents.len(), body));
                        }
                        snapshot.batch.agents.push(agent_row);
                        snapshot.batch.warming_agents.push(warming_row);
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        tracing::warn!(key = %row.0, session = %row.2, agent = %agent_id, %error, "a keepalive agent could not be written")
                    }
                }
            }
            snapshot
                .agents
                .push(((row.clone(), agent_id.clone()), *revision));
        }
        for (row, revision) in &self.dirty.removed_sessions {
            snapshot.batch.removed_sessions.push(row.clone());
            snapshot.removed_sessions.push((row.clone(), *revision));
        }
        for ((row, agent_id), revision) in &self.dirty.removed_agents {
            snapshot.batch.removed_agents.push((
                row.0.clone(),
                row.1,
                row.2.clone(),
                agent_id.clone(),
            ));
            snapshot
                .removed_agents
                .push(((row.clone(), agent_id.clone()), *revision));
        }
        for (key, filed) in &self.dirty.facts {
            match &filed.value {
                Some(value) => snapshot.batch.facts.push(FactsRow {
                    key: key.clone(),
                    record_version: FACTS_RECORD_VERSION,
                    value: value.clone(),
                    seen_at: filed.seen_at,
                }),
                None => snapshot.batch.removed_facts.push(key.clone()),
            }
            snapshot.facts.push((key.clone(), filed.revision));
        }
        for ((row, kind), revision) in &self.dirty.fact_kinds {
            snapshot
                .batch
                .removed_fact_kinds
                .push((row.0.clone(), row.1, row.2.clone(), *kind));
            snapshot.fact_kinds.push(((row.clone(), *kind), *revision));
        }
        for (pool_id, revision) in &self.dirty.pools {
            snapshot
                .batch
                .counters
                .extend(self.warming.counter_rows(pool_id));
            snapshot.pools.push((pool_id.clone(), *revision));
        }
        snapshot.purge_counters_before = self.warming.purge_cutoff();
        snapshot.batch.purge_counters_before = self.warming.purge_cutoff();
        snapshot
    }

    /// The settlements a landed save carried go to the ledger's queue, which the flush drains once the lock is
    /// released.
    pub(crate) fn release_settled(&mut self, settled: Vec<Settlement>) {
        self.warming.settlements.extend(settled);
    }

    /// Retires what a landed save carried: an entry leaves only while its revision is the one saved, so a change
    /// since the snapshot keeps it dirty for the next flush. The body the save sealed or cleared is the registry's
    /// whatever moved since, while the mark asking for a seal leaves only with its row's entry, since a recipe set
    /// after the snapshot is a body this save did not carry.
    pub(crate) fn acknowledge(&mut self, snapshot: &Snapshot) {
        for ((row, agent_id), revision, sealed) in &snapshot.bodies {
            let unchanged =
                self.dirty.agents.get(&(row.clone(), agent_id.clone())) == Some(revision);
            if self
                .session_of(row)
                .is_some_and(|session| session.agents.contains_key(agent_id))
            {
                let key: SessionKey = (row.0.clone(), row.2.clone());
                self.warming.settle_body(&key, agent_id, *sealed, unchanged);
            }
        }
        fn retire<K: Ord>(dirty: &mut BTreeMap<K, u64>, saved: &[(K, u64)]) {
            for (key, revision) in saved {
                if dirty.get(key) == Some(revision) {
                    dirty.remove(key);
                }
            }
        }
        retire(&mut self.dirty.sessions, &snapshot.sessions);
        retire(&mut self.dirty.agents, &snapshot.agents);
        retire(&mut self.dirty.removed_sessions, &snapshot.removed_sessions);
        retire(&mut self.dirty.removed_agents, &snapshot.removed_agents);
        retire(&mut self.dirty.pools, &snapshot.pools);
        retire(&mut self.dirty.fact_kinds, &snapshot.fact_kinds);
        for (key, revision) in &snapshot.facts {
            if self
                .dirty
                .facts
                .get(key)
                .is_some_and(|filed| filed.revision == *revision)
            {
                self.dirty.facts.remove(key);
            }
        }
        self.warming.purge_landed(snapshot.purge_counters_before);
    }

    /// `discard` for a recipe the load does not keep. The generation stays: nothing from before the restart can
    /// land, so no late result needs fencing off, and a kept generation keeps the mod's next report from a 409.
    fn discard_at_load(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        wasted: Option<Wasted>,
    ) {
        let Changes(changes) = self
            .warming
            .discard(&self.sessions, key, agent_id, reason, wasted);
        self.apply_changes(Changes(
            changes
                .into_iter()
                .filter(|change| !matches!(change, Change::Ended { .. }))
                .collect(),
        ));
    }

    /// The rows the store answered, restored within the limits the file sets now rather than the ones the save ran
    /// under: sessions newest first, each with its main agent and then its other agents newest first, and the recipes
    /// of what was restored newest first while the retained bytes allow. What does not fit is disposed, its rows
    /// filed for deletion and each recipe among them counted as evicted. Agents without a session row, ended sessions,
    /// released records whose leave has passed, rows of an older version and rows this build cannot read go too, and so
    /// does each agent whose two rows do not both read, with both of its rows.
    fn restore_persisted(
        &mut self,
        EngineState {
            sessions,
            agents,
            warming_agents,
            counters,
        }: EngineState,
        now: i64,
    ) {
        let (max_agents, max_sessions, max_retained_bytes, timing) = (
            self.sessions.max_agents,
            self.sessions.max_sessions,
            self.warming.max_retained_bytes,
            self.warming.timing,
        );
        self.warming.restore_counters(counters);
        let mut by_session: BTreeMap<PersistKey, BTreeMap<String, AgentRows>> = BTreeMap::new();
        for row in agents {
            let rows = by_session
                .entry((
                    row.key_id.clone(),
                    row.key_generation,
                    row.session_id.clone(),
                ))
                .or_default()
                .entry(row.agent_id.clone())
                .or_default();
            rows.agent = Some(row);
        }
        for row in warming_agents {
            let rows = by_session
                .entry((
                    row.key_id.clone(),
                    row.key_generation,
                    row.session_id.clone(),
                ))
                .or_default()
                .entry(row.agent_id.clone())
                .or_default();
            rows.warming = Some(row);
        }
        let mut sessions = sessions;
        sessions.sort_by(|a, b| {
            b.updated_at.cmp(&a.updated_at).then_with(|| {
                (&a.key_id, a.key_generation, &a.session_id).cmp(&(
                    &b.key_id,
                    b.key_generation,
                    &b.session_id,
                ))
            })
        });
        let mut by_sessions = Disposed::default();
        let mut by_agents = Disposed::default();
        let mut by_bytes = 0usize;
        let mut dropped = 0usize;
        let mut unopened: Vec<(SessionKey, String)> = Vec::new();
        let mut held: Vec<(i64, SessionKey, String, Observation)> = Vec::new();
        // Raised at each insert, so it counts the agents of the session in the making before that session joins
        // the map.
        let mut agent_count = self.sessions.agent_count();
        let mut live_sessions = self.sessions.live_sessions();
        for row in sessions {
            let stored: PersistKey = (
                row.key_id.clone(),
                row.key_generation,
                row.session_id.clone(),
            );
            let agent_rows = by_session.remove(&stored).unwrap_or_default();
            // Counted for one line after the loop rather than one per row: every row an upgrade finds is one of them.
            if row.record_version < SESSION_RECORD_VERSION {
                dropped += 1;
                for agent_id in agent_rows.into_keys() {
                    self.file_removed_agent(stored.clone(), agent_id);
                }
                self.file_removed_session(stored);
                continue;
            }
            let payload = decode_session(&row);
            let Some(payload) = payload else {
                tracing::warn!(key = %row.key_id, session = %row.session_id, "a saved keepalive session could not be read and is dropped");
                self.file_removed_session(stored);
                continue;
            };
            let key: SessionKey = (row.key_id.clone(), row.session_id.clone());
            // An ended session is a tombstone for reports still in flight, and none of those survive the restart. A
            // second row under the same ids is an older generation's, which a newer one replaced.
            if payload.ended || self.sessions.entries.contains_key(&key) {
                self.file_removed_session(stored);
                continue;
            }
            // Read before any limit, so a main agent this build cannot read counts as none. An agent with one of its
            // two rows alone reads as no agent, and both of its rows go.
            let mut agents: Vec<(CompanionAgentRow, KeepaliveAgentRow, Option<Decoded>)> =
                Vec::new();
            for (agent_id, rows) in agent_rows {
                let AgentRows {
                    agent: Some(agent_row),
                    warming: Some(warming_row),
                } = rows
                else {
                    tracing::warn!(key = %row.key_id, session = %row.session_id, agent = %agent_id, "a saved keepalive agent could not be read and is dropped");
                    self.file_removed_agent(stored.clone(), agent_id);
                    continue;
                };
                let decoded = decode_agent(&agent_row, &warming_row, timing);
                if decoded.is_none() {
                    tracing::warn!(key = %row.key_id, session = %row.session_id, agent = %agent_id, "a saved keepalive agent could not be read and is dropped");
                }
                agents.push((agent_row, warming_row, decoded));
            }
            // Every report the mod sends names the main agent, so a session restored without one could never be
            // re-adopted.
            if !agents.iter().any(|(agent_row, _, decoded)| {
                agent_row.agent_id == MAIN_AGENT && decoded.is_some()
            }) {
                tracing::warn!(key = %row.key_id, session = %row.session_id, "a saved keepalive session without a readable main agent is dropped");
                self.file_removed_session(stored);
                continue;
            }
            let mut session = payload.restore(now);
            let mut warming_agents = BTreeMap::new();
            // A session that has sent no real request the caps admit holds no slot of either cap, so it passes both
            // bounds and counts toward neither.
            let holds_slot = session.holds_slot();
            let over = if !holds_slot {
                None
            } else if live_sessions >= max_sessions {
                Some(&mut by_sessions)
            } else if agent_count >= max_agents {
                Some(&mut by_agents)
            } else {
                None
            };
            if let Some(tally) = over {
                tally.sessions += 1;
                for (_, warming_row, decoded) in agents {
                    self.warming.dispose_loaded(
                        &key,
                        &warming_row,
                        decoded.map(|(_, warming, _)| warming),
                        tally,
                    );
                }
                self.file_removed_session(stored);
                continue;
            }
            agents.sort_by(|(a, _, _), (b, _, _)| {
                (b.agent_id == MAIN_AGENT)
                    .cmp(&(a.agent_id == MAIN_AGENT))
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
                    .then_with(|| a.agent_id.cmp(&b.agent_id))
            });
            for (agent_row, mut warming_row, decoded) in agents {
                let Some((agent, mut warming, observation)) = decoded else {
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                };
                // The sweep has not run yet at the load, and a leave that passed while the router was down is already
                // due, so the record goes here, before the cap read below can give it a live agent's slot.
                if agent.leave_passed(now) {
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                }
                // Read off the row rather than the opened body, so a blob that no longer opens is cleared by the
                // agent's next save.
                warming.body_saved = warming_row.body_size > 0;
                if holds_slot && agent_count >= max_agents {
                    self.warming
                        .dispose_loaded(&key, &warming_row, Some(warming), &mut by_agents);
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                }
                self.sessions.next_incarnation =
                    self.sessions.next_incarnation.max(agent.incarnation);
                let agent_id = agent_row.agent_id.clone();
                // The observation's restore reads the row's other columns alone, so the opened prompt moves out.
                match (observation, warming_row.body.take()) {
                    (None, _) => {}
                    (Some(observation), Some(body)) => {
                        match observation.restore(&warming_row, body) {
                            Some(observation) => held.push((
                                warming_row.updated_at,
                                key.clone(),
                                agent_id.clone(),
                                observation,
                            )),
                            None => unopened.push((key.clone(), agent_id.clone())),
                        }
                    }
                    // The prompt is sealed under the storage key and bound to its row. One that no longer opens is
                    // gone, and the agent keeps everything else.
                    (Some(_), None) => unopened.push((key.clone(), agent_id.clone())),
                }
                session.agents.insert(agent_id.clone(), agent);
                warming_agents.insert(agent_id, warming);
                if holds_slot {
                    agent_count += 1;
                }
            }
            if holds_slot {
                live_sessions += 1;
            }
            self.sessions.entries.insert(key.clone(), session);
            self.warming.agents.insert(key, warming_agents);
        }
        // Agent rows of either table whose session row is gone or was dropped above.
        for (stored, rows) in by_session {
            for agent_id in rows.into_keys() {
                self.file_removed_agent(stored.clone(), agent_id);
            }
        }
        for (key, agent_id) in unopened {
            tracing::info!(key = %key.0, session = %key.1, agent = %agent_id, "a saved keepalive prompt could not be opened and is dropped");
            self.discard_at_load(&key, &agent_id, Reason::Reset, None);
        }
        held.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| (&a.1, &a.2).cmp(&(&b.1, &b.2))));
        for (_, key, agent_id, observation) in held {
            let size = observation.recipe.size;
            self.warming.retained += size;
            let warming = warming_mut(&mut self.warming.agents, &key, &agent_id)
                .expect("the agent the load just restored");
            let expires = warming.expires;
            warming.current = Some(observation);
            if let Some(expires) = expires.filter(|expires| *expires <= now) {
                tracing::info!(
                    key = %key.0,
                    session = %key.1,
                    agent = %agent_id,
                    expires = %phoebis_store::utc_stamp(expires),
                    "a keepalive recipe's entry lapsed while the router was down"
                );
                self.discard_at_load(
                    &key,
                    &agent_id,
                    Reason::ExpiredAtRestart,
                    Some(Wasted::Expired),
                );
                continue;
            }
            if self.warming.retained > max_retained_bytes {
                by_bytes += 1;
                self.discard_at_load(&key, &agent_id, Reason::Evicted, Some(Wasted::Evicted));
            }
        }
        if dropped > 0 {
            tracing::warn!(
                dropped,
                "saved keepalive sessions of an older record version are dropped, since their probes carry no funding or no usage to settle under"
            );
        }
        if by_sessions.sessions > 0 {
            tracing::info!(
                sessions = by_sessions.sessions,
                agents = by_sessions.agents,
                recipes = by_sessions.recipes,
                max_sessions,
                "keepalive sessions past max_sessions were not restored"
            );
        }
        if by_agents.agents > 0 {
            tracing::info!(
                sessions = by_agents.sessions,
                agents = by_agents.agents,
                recipes = by_agents.recipes,
                max_agents,
                "keepalive agents past max_agents were not restored"
            );
        }
        if by_bytes > 0 {
            tracing::info!(
                recipes = by_bytes,
                max_retained_bytes,
                "keepalive recipes past max_retained_bytes were not restored"
            );
        }
    }
}

/// A stored value in the raw form `T`, `None` for a row that held no value to read or whose value does not parse.
fn parsed<T: for<'de> Deserialize<'de>>(value: Option<Vec<u8>>) -> Option<T> {
    serde_json::from_slice(&value?).ok()
}

impl Engine {
    /// Reads back what the last process saved, at start and before the scheduler ticks, bounded by the current limits.
    /// The rows the load drops or rewrites are saved before it returns, so the store and the map agree from here.
    pub async fn load_persisted(self: &Arc<Self>) {
        if !self.lock().sessions.entries.is_empty() {
            tracing::warn!("the keepalive's saved state is read once, at start, and this engine already holds sessions");
            return;
        }
        let store = self.parts.store.clone();
        let loaded = match tokio::task::spawn_blocking(move || store.load_engine_state()).await {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                tracing::warn!(%error, "the keepalive's saved state could not be read, so the engine starts empty");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "the keepalive's saved state could not be read, so the engine starts empty");
                return;
            }
        };
        let now = self.now();
        {
            let mut inner = self.lock();
            // A load over sessions already held would file the removal of rows the engine holds live.
            if !inner.sessions.entries.is_empty() {
                return;
            }
            inner.restore_persisted(loaded, now);
            // The registry first, then the ledger: the settlements wait for the save that carries each ended recipe's
            // agent without the probes they settle, so a failed save sends nothing and a crash between the save and
            // the ledger's write leaves a wasted probe unwritten rather than written again by the next load.
            inner.warming.hold_settlements_for_save();
        }
        self.flush_persisted().await;
    }

    /// The stored facts of `sessions`, each `(key_id, key_generation, session_id)`, read from the registry on the
    /// blocking pool. A session with no stored set has no entry. A row that does not open, a value that does not parse
    /// and a row of another record version read as no fact, and so does each row where the read itself fails.
    pub async fn stored_facts(
        &self,
        sessions: Vec<(String, i64, String)>,
    ) -> BTreeMap<(String, i64, String), StoredSessionFacts> {
        let store = self.parts.store.clone();
        let rows = match tokio::task::spawn_blocking(move || store.load_companion_facts(&sessions))
            .await
        {
            Ok(Ok(rows)) => rows,
            Ok(Err(error)) => {
                tracing::warn!(%error, "the stored facts could not be read, so each session reads none");
                return BTreeMap::new();
            }
            Err(error) => {
                tracing::warn!(%error, "the stored facts could not be read, so each session reads none");
                return BTreeMap::new();
            }
        };
        // The title and the project rest in two rows, so a session's own set is joined ahead of the rules.
        let mut own: BTreeMap<PersistKey, RawSessionFacts> = BTreeMap::new();
        let mut stored: BTreeMap<PersistKey, StoredSessionFacts> = BTreeMap::new();
        for row in rows {
            let FactsKey {
                key_id,
                key_generation,
                session_id,
                kind,
                agent_id,
            } = row.key;
            let id = (key_id, key_generation, session_id);
            let value = (row.record_version == FACTS_RECORD_VERSION)
                .then_some(row.value)
                .flatten();
            let read = match kind {
                FactsKind::Agents => parsed::<RawAgentFacts>(value).map(|raw| {
                    let facts = AgentFacts::of(raw);
                    if !facts.is_empty() {
                        stored
                            .entry(id.clone())
                            .or_default()
                            .agents
                            .push((agent_id, facts));
                    }
                }),
                FactsKind::Project => parsed::<RawSessionFacts>(value).map(|raw| {
                    // Whole, so a member that joins the raw form takes a decision here.
                    let RawSessionFacts {
                        branch,
                        cwd,
                        project,
                        repository,
                        repository_root,
                        title: _,
                    } = raw;
                    let joined = own.entry(id.clone()).or_default();
                    joined.branch = branch;
                    joined.cwd = cwd;
                    joined.project = project;
                    joined.repository = repository;
                    joined.repository_root = repository_root;
                }),
                FactsKind::Title => parsed::<RawSessionFacts>(value)
                    .map(|raw| own.entry(id.clone()).or_default().title = raw.title),
            };
            if read.is_none() {
                tracing::warn!(key = %id.0, session = %id.2, "a stored set of reported facts could not be read, so it reads as no fact");
            }
        }
        for (id, raw) in own {
            stored.entry(id).or_default().session = SessionFacts::of(raw);
        }
        stored
    }

    /// A snapshot, its save and its acknowledgment, on the blocking pool under the flush lock whole: two flushes never
    /// overlap, an older batch never follows a newer one to the store, and a flush whose caller went away still runs
    /// to its end, so the shutdown's flush queues behind it. A failed save keeps every row dirty for the next flush
    /// and holds back the settlements waiting on it. A landed save releases them, and they reach the ledger here.
    pub async fn flush_persisted(self: &Arc<Self>) {
        let engine = self.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || engine.flush_blocking()).await {
            tracing::warn!(%error, "the keepalive's flush did not finish");
        }
        self.flush();
    }

    /// Asks the save worker for a save. The worker runs one save at a time. A call that arrives while a save runs is
    /// kept as one stored wakeup, so the worker saves once more and no caller holds a thread while it waits. The worker
    /// starts with the first call, inside the runtime that serves the route, and ends when the engine's token fires.
    pub(crate) fn save_soon(self: &Arc<Self>) {
        self.save_worker.get_or_init(|| {
            let wanted = self.save_wanted.clone();
            let cancel = self.cancel.clone();
            let engine = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    // The token first, as in the periodic flusher: a wakeup that is stored when it fires starts no
                    // further save, so the shutdown's save stays the last one that starts.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        _ = wanted.notified() => {}
                    }
                    // The worker holds the engine for the length of one save alone, so it keeps no engine alive.
                    let Some(engine) = engine.upgrade() else {
                        return;
                    };
                    engine.flush_persisted().await;
                }
            });
        });
        self.save_wanted.notify_one();
    }

    fn flush_blocking(&self) {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.hooks.raise(&self.hooks.persist_entries);
        // A panic inside a flush poisons the lock, which orders flushes and guards no invariant of its own.
        let _flushing = self
            .flush_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = self.now();
        let mut snapshot = self.lock().persist_snapshot(now);
        #[cfg(any(test, feature = "debug-hooks"))]
        self.hooks.raise(&self.hooks.persist_snapshots);
        let settled = std::mem::take(&mut snapshot.settled);
        // Nothing to write means the registry already holds every row, which releases the held settlements too.
        if snapshot.is_empty() {
            self.lock().release_settled(settled);
            return;
        }
        if snapshot.batch.is_empty() {
            let mut inner = self.lock();
            inner.acknowledge(&snapshot);
            inner.release_settled(settled);
            return;
        }
        for (index, body) in snapshot.seals.drain(..) {
            snapshot.batch.warming_agents[index].body_write = BodyWrite::Seal(body.to_vec());
        }
        match self.parts.store.save_engine_state(&snapshot.batch) {
            Ok(()) => {
                let mut inner = self.lock();
                inner.acknowledge(&snapshot);
                inner.release_settled(settled);
                if std::mem::take(&mut inner.persist_failing) {
                    tracing::info!("the keepalive's state reaches the registry again");
                }
            }
            Err(error) => {
                let mut inner = self.lock();
                inner.warming.return_unsaved_settlements(settled);
                if !inner.persist_failing {
                    inner.persist_failing = true;
                    tracing::warn!(%error, "the keepalive's state could not be written to the registry, so the engine keeps it and retries at every flush");
                }
            }
        }
    }

    /// The periodic flusher. It ends when the engine's token fires and is joined rather than aborted, so a save it
    /// started is acknowledged before the task returns.
    pub fn spawn_persist_flush(self: Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick completes at once, before anything could have changed.
            tick.tick().await;
            loop {
                // The token first, so a tick that is due when it fires starts no further flush.
                tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return,
                    _ = tick.tick() => self.flush_persisted().await,
                }
            }
        })
    }

    /// The shutdown's first step: the scheduler, the flusher and the probes in flight are canceled, and the intake
    /// closes, so a request's event or a report that lands later is refused and counted rather than applied after the
    /// last save. Taking the lock is the drain: an update that already holds it finishes before the flag is set.
    pub fn quiesce(&self) {
        self.cancel.cancel();
        self.lock().closed = true;
    }

    /// How many flushes have read their batch. Compiled only under `debug-hooks`, as `router_views` is.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn persist_snapshots(&self) -> u64 {
        self.hooks.persist_snapshots.load(Ordering::SeqCst)
    }

    /// How many flushes have entered, counted before the flush lock. Compiled only under `debug-hooks`.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn persist_entries(&self) -> u64 {
        self.hooks.persist_entries.load(Ordering::SeqCst)
    }

    /// Resolves once `entries` flushes have entered and `snapshots` have read their batch. A flush raises the counts
    /// on its own thread, so this needs no timer, which a runtime whose workers wait behind a held save would not
    /// drive. Compiled only under `debug-hooks`.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub async fn persist_reached(&self, entries: u64, snapshots: u64) {
        phoebis_routing::wait_until(
            &self.hooks.persist_progress,
            None,
            || self.persist_entries() >= entries && self.persist_snapshots() >= snapshots,
            || {},
        )
        .await;
    }
}
