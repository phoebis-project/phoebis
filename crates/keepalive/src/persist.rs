//! The engine's state across a restart: its sessions, its agents with their retained prompts, and the idle-age
//! counters, written behind the map to the registry and read back once at start. The map stays the truth while the
//! engine runs, and the store is what the next process starts from. The relay's session memory has the same shape: a
//! dirty set under revisions, one flush at a time on the blocking pool, and an acknowledgment that retires only what
//! the save carried.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use phoebis_ledger::Speed;
use phoebis_store::{IdleReturnsRow, KeepaliveAgentRow, KeepaliveBatch, KeepaliveSessionRow};
use serde::{Deserialize, Serialize};

use crate::config::{KeepaliveLimits, KeepalivePolicy, KeepaliveTiming};
use crate::engine::{
    Agent, AgentState, Engine, Guard, IdleEpisode, IdleReturnBucket, Inner, Kept, Observation,
    ParkKind, PriorCoverage, Reason, SavingsUnit, Session, SessionKey, Wasted, MAIN_AGENT,
};
use crate::recipe::Recipe;
use crate::usage::ProbeUsage;

/// The payloads' shape. Nothing is released, so no stored row of another version exists to read.
pub(crate) const RECORD_VERSION: u32 = 1;

/// A session row's key in the store: key id, key generation, session id.
pub(crate) type PersistKey = (String, i64, String);

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
}

/// Every field of `Session` but its agents, which have rows of their own.
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
    pub owner_id: Option<String>,
    pub last_report_at: i64,
    pub last_real_at: Option<i64>,
}

/// Every field of `Agent` but `in_flight`, with the observation's fields and the recipe's own beside it. The prompt,
/// its header block, its size, its speed and the dispatch TTL are the row's columns, so the body is sealed apart
/// from the payload.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedAgent {
    pub id: String,
    pub key_id: String,
    pub key_generation: i64,
    pub incarnation: u64,
    pub state: AgentState,
    pub deadline: Option<i64>,
    pub park_kind: Option<ParkKind>,
    pub wake_at: Option<i64>,
    pub park_began: Option<i64>,
    pub park_ended_at: Option<i64>,
    pub idle_episode: Option<PersistedEpisode>,
    pub idle_window_floor: Option<i64>,
    pub last_prefix: Option<(String, String)>,
    pub generation: u64,
    pub current: Option<PersistedObservation>,
    pub expires: Option<i64>,
    #[serde(with = "exact")]
    pub jitter_frac: f64,
    pub retry_at: Option<i64>,
    pub policy: KeepalivePolicy,
    pub timing: KeepaliveTiming,
    pub last_activity: i64,
    pub cache_started: i64,
    pub ttl_ms: i64,
    pub idle_ticks: u64,
    pub cold: u32,
    pub failures: u32,
    pub account: Option<String>,
    pub pool_id: String,
    pub canonical_model: String,
    pub reason: String,
    pub last_guard: Option<Guard>,
    pub hold_counted: bool,
    pub paused: bool,
    pub probes: u64,
    pub migrations: u64,
    pub usage: PersistedUsage,
    #[serde(with = "exact_list")]
    pub unanswered: Vec<f64>,
    pub left_fast: Option<bool>,
    pub kept: Vec<PersistedKept>,
    pub covered_since_real: bool,
    pub covered_account: Option<String>,
    pub covered_unit: Option<String>,
    pub covered_fast: bool,
    pub covered_until: Option<i64>,
    pub last_real_at: Option<i64>,
    pub last_stop: Option<Wasted>,
    pub last_presence_at: Option<i64>,
    pub last_probe_at: Option<i64>,
    pub probes_today: u64,
    pub credited_today: u64,
    pub counters_day: i64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedEpisode {
    pub started: i64,
    pub bucket: Option<usize>,
    pub anchor: Option<(String, String)>,
    pub prompt_at: Option<i64>,
    pub pool_key: String,
}

/// `poisoned` is written as it stood and read back false: the gap that poisoned the observation is this process's,
/// and the next one starts with no backlog behind it.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedObservation {
    pub id: String,
    pub attempt: u8,
    pub started: i64,
    pub ready: bool,
    pub expected: u64,
    pub ttl_ms: i64,
    pub prior: Option<PersistedPrior>,
    pub poisoned: bool,
    pub recipe: PersistedRecipe,
}

/// The recipe's fields beside the prompt and its header block: the hijack guard reads the two hashes, and the probe's
/// TTL reads the two TTLs.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedRecipe {
    pub model: String,
    pub ttl_ms: i64,
    pub longest_ttl_ms: i64,
    pub static_hash: Option<String>,
    pub tail_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedPrior {
    pub account: Option<String>,
    pub covered: bool,
    pub unit: Option<String>,
    pub model: String,
    pub fast: bool,
    pub real_at: i64,
    pub ttl_ms: i64,
    pub until: Option<i64>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedUsage {
    pub read: u64,
    pub write: u64,
    pub output: u64,
    pub five_minutes: u64,
    pub one_hour: u64,
    pub input: u64,
    pub speed: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedKept {
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub account: Option<String>,
    pub fast: bool,
    #[serde(with = "exact_list")]
    pub costs: Vec<f64>,
    pub entry_expires: i64,
}

/// A float as the text `f64`'s `Display` writes, which `str::parse` reads back to the same bits. serde_json's own float
/// parser can land a unit in the last place off, and a jitter that moves across a restart is not the one that was
/// saved.
mod exact {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// `exact` over a list.
mod exact_list {
    use serde::{de::Error, ser::SerializeSeq, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(values: &[f64], serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(values.len()))?;
        for value in values {
            seq.serialize_element(&value.to_string())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<f64>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .iter()
            .map(|text| text.parse().map_err(D::Error::custom))
            .collect()
    }
}

fn unit_word(unit: SavingsUnit) -> String {
    unit.as_str().to_string()
}

fn unit_of(word: &str) -> Option<SavingsUnit> {
    match word {
        "usd" => Some(SavingsUnit::Usd),
        "window_share" => Some(SavingsUnit::WindowShare),
        _ => None,
    }
}

/// A word the payload carries for an optional unit: `None` reads as none, and a word this build does not know
/// refuses the row.
fn optional_unit(word: Option<String>) -> Option<Option<SavingsUnit>> {
    match word {
        None => Some(None),
        Some(word) => unit_of(&word).map(Some),
    }
}

fn reason_of(word: &str) -> Option<Reason> {
    Reason::ALL
        .iter()
        .copied()
        .find(|reason| reason.as_str() == word)
}

impl PersistedSession {
    fn of(session: &Session) -> PersistedSession {
        PersistedSession {
            key_id: session.key_id.clone(),
            key_generation: session.key_generation,
            session_id: session.session_id.clone(),
            instance: session.instance.clone(),
            sequence: session.sequence,
            lease: session.lease,
            attended: session.attended,
            ended: session.ended,
            owner_id: session.owner_id.clone(),
            last_report_at: session.last_report_at,
            last_real_at: session.last_real_at,
        }
    }

    /// Silent until the mod's next heartbeat or registration re-adopts it: the lease at zero, so the next report
    /// answers `instance_ended` and the same instance registers again, and the sequence and the owner kept, so that
    /// registration is the one the mod would send next.
    fn restore(self) -> Session {
        Session {
            key_id: self.key_id,
            key_generation: self.key_generation,
            session_id: self.session_id,
            instance: self.instance,
            sequence: self.sequence,
            lease: 0,
            attended: false,
            ended: false,
            owner_id: self.owner_id,
            last_report_at: self.last_report_at,
            last_real_at: self.last_real_at,
            agents: BTreeMap::new(),
        }
    }
}

impl PersistedAgent {
    fn of(agent: &Agent) -> PersistedAgent {
        PersistedAgent {
            id: agent.id.clone(),
            key_id: agent.key_id.clone(),
            key_generation: agent.key_generation,
            incarnation: agent.incarnation,
            state: agent.state,
            deadline: agent.deadline,
            park_kind: agent.park_kind,
            wake_at: agent.wake_at,
            park_began: agent.park_began,
            park_ended_at: agent.park_ended_at,
            idle_episode: agent.idle_episode.as_ref().map(|episode| PersistedEpisode {
                started: episode.started,
                bucket: episode.bucket,
                anchor: episode.anchor.clone(),
                prompt_at: episode.prompt_at,
                pool_key: episode.pool_key.clone(),
            }),
            idle_window_floor: agent.idle_window_floor,
            last_prefix: agent.last_prefix.clone(),
            generation: agent.generation,
            current: agent.current.as_ref().map(|o| PersistedObservation {
                id: o.id.clone(),
                attempt: o.attempt,
                started: o.started,
                ready: o.ready,
                expected: o.expected,
                ttl_ms: o.ttl_ms,
                prior: o.prior.as_ref().map(|prior| PersistedPrior {
                    account: prior.account.clone(),
                    covered: prior.covered,
                    unit: prior.unit.map(unit_word),
                    model: prior.model.clone(),
                    fast: prior.fast,
                    real_at: prior.real_at,
                    ttl_ms: prior.ttl_ms,
                    until: prior.until,
                }),
                poisoned: o.poisoned,
                recipe: PersistedRecipe {
                    model: o.recipe.model.clone(),
                    ttl_ms: o.recipe.ttl_ms,
                    longest_ttl_ms: o.recipe.longest_ttl_ms,
                    static_hash: o.recipe.static_hash.clone(),
                    tail_hash: o.recipe.tail_hash.clone(),
                },
            }),
            expires: agent.expires,
            jitter_frac: agent.jitter_frac,
            retry_at: agent.retry_at,
            policy: agent.policy.clone(),
            timing: agent.timing,
            last_activity: agent.last_activity,
            cache_started: agent.cache_started,
            ttl_ms: agent.ttl_ms,
            idle_ticks: agent.idle_ticks,
            cold: agent.cold,
            failures: agent.failures,
            account: agent.account.clone(),
            pool_id: agent.pool_id.clone(),
            canonical_model: agent.canonical_model.clone(),
            reason: agent.reason.as_str().to_string(),
            last_guard: agent.last_guard,
            hold_counted: agent.hold_counted,
            paused: agent.paused,
            probes: agent.probes,
            migrations: agent.migrations,
            usage: PersistedUsage {
                read: agent.usage.read,
                write: agent.usage.write,
                output: agent.usage.output,
                five_minutes: agent.usage.five_minutes,
                one_hour: agent.usage.one_hour,
                input: agent.usage.input,
                speed: agent.usage.speed.map(|speed| {
                    match speed {
                        Speed::Fast => "fast",
                        Speed::Standard => "standard",
                    }
                    .to_string()
                }),
            },
            unanswered: agent.unanswered.clone(),
            left_fast: agent.left_fast,
            kept: agent
                .kept
                .iter()
                .map(|kept| PersistedKept {
                    key_generation: kept.key_generation,
                    pool_id: kept.pool_id.clone(),
                    canonical_model: kept.canonical_model.clone(),
                    account: kept.account.clone(),
                    fast: kept.fast,
                    costs: kept.costs.clone(),
                    entry_expires: kept.entry_expires,
                })
                .collect(),
            covered_since_real: agent.covered_since_real,
            covered_account: agent.covered_account.clone(),
            covered_unit: agent.covered_unit.map(unit_word),
            covered_fast: agent.covered_fast,
            covered_until: agent.covered_until,
            last_real_at: agent.last_real_at,
            last_stop: agent.last_stop,
            last_presence_at: agent.last_presence_at,
            last_probe_at: agent.last_probe_at,
            probes_today: agent.probes_today,
            credited_today: agent.credited_today,
            counters_day: agent.counters_day,
        }
    }

    /// The agent the payload describes, without its observation, and the observation's own fields apart, since the
    /// recipe they ride on comes from the row's columns. `None` when a word in the payload is one this build does not
    /// know. The timing is the engine's rather than the payload's: it is the binary's fixed numbers, which the build a
    /// restart brings may have changed.
    fn restore(self, timing: KeepaliveTiming) -> Option<(Agent, Option<PersistedObservation>)> {
        let mut agent = Agent::new(
            &self.id,
            &self.key_id,
            self.incarnation,
            self.last_activity,
            timing,
        );
        agent.key_generation = self.key_generation;
        agent.state = self.state;
        agent.deadline = self.deadline;
        agent.park_kind = self.park_kind;
        agent.wake_at = self.wake_at;
        agent.park_began = self.park_began;
        agent.park_ended_at = self.park_ended_at;
        agent.idle_episode = self.idle_episode.map(|episode| IdleEpisode {
            started: episode.started,
            bucket: episode.bucket,
            anchor: episode.anchor,
            prompt_at: episode.prompt_at,
            pool_key: episode.pool_key,
        });
        agent.idle_window_floor = self.idle_window_floor;
        agent.last_prefix = self.last_prefix;
        agent.generation = self.generation;
        agent.expires = self.expires;
        agent.jitter_frac = self.jitter_frac;
        agent.retry_at = self.retry_at;
        agent.policy = self.policy;
        agent.cache_started = self.cache_started;
        agent.ttl_ms = self.ttl_ms;
        agent.idle_ticks = self.idle_ticks;
        agent.cold = self.cold;
        agent.failures = self.failures;
        agent.account = self.account;
        agent.pool_id = self.pool_id;
        agent.canonical_model = self.canonical_model;
        agent.reason = reason_of(&self.reason)?;
        agent.last_guard = self.last_guard;
        agent.hold_counted = self.hold_counted;
        agent.paused = self.paused;
        agent.probes = self.probes;
        agent.migrations = self.migrations;
        agent.usage = ProbeUsage {
            read: self.usage.read,
            write: self.usage.write,
            output: self.usage.output,
            five_minutes: self.usage.five_minutes,
            one_hour: self.usage.one_hour,
            input: self.usage.input,
            speed: self
                .usage
                .speed
                .as_deref()
                .map(|word| Speed::of(Some(word))),
        };
        agent.unanswered = self.unanswered;
        agent.left_fast = self.left_fast;
        agent.kept = self
            .kept
            .into_iter()
            .map(|kept| Kept {
                key_generation: kept.key_generation,
                pool_id: kept.pool_id,
                canonical_model: kept.canonical_model,
                account: kept.account,
                fast: kept.fast,
                costs: kept.costs,
                entry_expires: kept.entry_expires,
            })
            .collect();
        agent.covered_since_real = self.covered_since_real;
        agent.covered_account = self.covered_account;
        agent.covered_unit = optional_unit(self.covered_unit)?;
        agent.covered_fast = self.covered_fast;
        agent.covered_until = self.covered_until;
        agent.last_real_at = self.last_real_at;
        agent.last_stop = self.last_stop;
        agent.last_presence_at = self.last_presence_at;
        agent.last_probe_at = self.last_probe_at;
        agent.probes_today = self.probes_today;
        agent.credited_today = self.credited_today;
        agent.counters_day = self.counters_day;
        Some((agent, self.current))
    }
}

impl PersistedObservation {
    /// The observation over the recipe the row's columns carry: the opened prompt, the header block, the size, the
    /// speed and the dispatch TTL.
    fn restore(self, row: &KeepaliveAgentRow, body: Vec<u8>) -> Option<Observation> {
        let headers: Vec<(String, String)> = serde_json::from_str(&row.headers).ok()?;
        let prior = match self.prior {
            None => None,
            Some(prior) => Some(PriorCoverage {
                account: prior.account,
                covered: prior.covered,
                unit: optional_unit(prior.unit)?,
                model: prior.model,
                fast: prior.fast,
                real_at: prior.real_at,
                ttl_ms: prior.ttl_ms,
                until: prior.until,
            }),
        };
        Some(Observation {
            id: self.id,
            attempt: self.attempt,
            started: self.started,
            recipe: Recipe {
                body,
                headers,
                model: self.recipe.model,
                ttl_ms: self.recipe.ttl_ms,
                longest_ttl_ms: self.recipe.longest_ttl_ms,
                size: row.body_size,
                static_hash: self.recipe.static_hash,
                tail_hash: self.recipe.tail_hash,
                fast: row.fast,
            },
            ready: self.ready,
            expected: self.expected,
            ttl_ms: self.ttl_ms,
            prior,
            poisoned: false,
            dispatched_ms: row.dispatched_ms,
        })
    }
}

/// One session row as the store keeps it.
fn session_row(
    key: &PersistKey,
    session: &Session,
    now: i64,
) -> Result<KeepaliveSessionRow, serde_json::Error> {
    Ok(KeepaliveSessionRow {
        key_id: key.0.clone(),
        key_generation: key.1,
        session_id: key.2.clone(),
        record_version: RECORD_VERSION,
        payload: serde_json::to_string(&PersistedSession::of(session))?,
        updated_at: now,
    })
}

/// One agent row as the store keeps it: the payload, and the recipe's prompt and columns when one is retained.
fn agent_row(
    key: &PersistKey,
    agent: &Agent,
    now: i64,
) -> Result<KeepaliveAgentRow, serde_json::Error> {
    let recipe = agent.current.as_ref().map(|o| &o.recipe);
    let no_headers: &[(String, String)] = &[];
    Ok(KeepaliveAgentRow {
        key_id: key.0.clone(),
        key_generation: key.1,
        session_id: key.2.clone(),
        agent_id: agent.id.clone(),
        record_version: RECORD_VERSION,
        payload: serde_json::to_string(&PersistedAgent::of(agent))?,
        body: recipe.map(|r| r.body.clone()),
        headers: serde_json::to_string(recipe.map_or(no_headers, |r| r.headers.as_slice()))?,
        body_size: recipe.map_or(0, |r| r.size),
        dispatched_ms: agent.current.as_ref().and_then(|o| o.dispatched_ms),
        fast: recipe.is_some_and(|r| r.fast),
        updated_at: now,
    })
}

/// What one flush carries, with the revision each row was read at, so the acknowledgment retires only those.
#[derive(Default)]
pub(crate) struct Snapshot {
    pub batch: KeepaliveBatch,
    sessions: Vec<(PersistKey, u64)>,
    agents: Vec<((PersistKey, String), u64)>,
    removed_sessions: Vec<(PersistKey, u64)>,
    removed_agents: Vec<((PersistKey, String), u64)>,
    pools: Vec<(String, u64)>,
}

impl Snapshot {
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.agents.is_empty()
            && self.removed_sessions.is_empty()
            && self.removed_agents.is_empty()
            && self.pools.is_empty()
    }
}

/// The rows the store answered at start, sessions, agents and counters.
type Loaded = (
    Vec<KeepaliveSessionRow>,
    Vec<KeepaliveAgentRow>,
    Vec<IdleReturnsRow>,
);

/// An agent row read back: the agent, and its observation's fields when it carried a recipe.
type Decoded = (Agent, Option<PersistedObservation>);

/// The agent a row describes, `None` when its version or a word in its payload is one this build does not read.
fn decode_agent(row: &KeepaliveAgentRow, timing: KeepaliveTiming) -> Option<Decoded> {
    (row.record_version == RECORD_VERSION)
        .then(|| serde_json::from_str::<PersistedAgent>(&row.payload).ok())
        .flatten()
        .and_then(|payload| payload.restore(timing))
}

/// What the load left out to stay within the limits the file sets now, for the one line each limit logs.
#[derive(Default)]
struct Disposed {
    sessions: usize,
    agents: usize,
    recipes: usize,
}

impl Inner {
    fn bump(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }

    fn persist_key(&self, key: &SessionKey) -> Option<PersistKey> {
        self.sessions
            .get(key)
            .map(|session| (key.0.clone(), session.key_generation, key.1.clone()))
    }

    fn session_of(&self, key: &PersistKey) -> Option<&Session> {
        self.sessions
            .get(&(key.0.clone(), key.2.clone()))
            .filter(|session| session.key_generation == key.1)
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

    /// The agent's row is dirty under its session's generation now.
    pub(crate) fn touch_agent(&mut self, key: &SessionKey, agent_id: &str) {
        let held = self
            .sessions
            .get(key)
            .is_some_and(|session| session.agents.contains_key(agent_id));
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

    /// The agent's row goes at the next save, under the generation its session is held under: called while the agent
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
        let agents: Vec<String> = self.sessions[key].agents.keys().cloned().collect();
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

    /// The batch the dirty sets name, read as the map stands now, each row with the revision it was read at. Nothing
    /// is cleared here: `acknowledge` does that once the save has landed. A dirty row whose record left the map without
    /// a removal, which only `close` does, is carried by nothing and retired with the rest.
    pub(crate) fn persist_snapshot(&self, now: i64) -> Snapshot {
        let mut snapshot = Snapshot::default();
        for (row, revision) in &self.dirty.sessions {
            if let Some(session) = self.session_of(row) {
                match session_row(row, session, now) {
                    Ok(session_row) => snapshot.batch.sessions.push(session_row),
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
                match agent_row(row, agent, now) {
                    Ok(agent_row) => snapshot.batch.agents.push(agent_row),
                    Err(error) => {
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
        for (pool_id, revision) in &self.dirty.pools {
            if let Some(counters) = self.idle_returns.get(pool_id) {
                for (bucket, counter) in counters.buckets.iter().enumerate() {
                    snapshot.batch.counters.push(IdleReturnsRow {
                        pool_id: pool_id.clone(),
                        bucket: bucket as u32,
                        reached: counter.reached,
                        returned: counter.returned,
                    });
                }
            }
            snapshot.pools.push((pool_id.clone(), *revision));
        }
        snapshot
    }

    /// Retires what a landed save carried: an entry leaves only while its revision is the one saved, so a change
    /// since the snapshot keeps it dirty for the next flush.
    pub(crate) fn acknowledge(&mut self, snapshot: &Snapshot) {
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
    }

    /// An agent row a limit leaves out of the load. Its recipe counts as evicted, and the probes it left unanswered and
    /// the batches a request kept settle under `evicted`, as they do when the retained bytes evict a recipe through
    /// `discard`. The row itself goes with its session's removal or the caller's.
    fn dispose_loaded(
        &mut self,
        key: &SessionKey,
        row: &KeepaliveAgentRow,
        decoded: Option<Decoded>,
        tally: &mut Disposed,
    ) {
        tally.agents += 1;
        if row.body_size > 0 {
            tally.recipes += 1;
            *self.reasons.entry(Reason::Evicted).or_default() += 1;
        }
        let Some((mut agent, _)) = decoded else {
            return;
        };
        let fast = agent.left_fast.take().unwrap_or(row.fast);
        let costs = std::mem::take(&mut agent.unanswered);
        if !costs.is_empty() {
            self.settlements
                .push(Kept::of(&agent, fast, costs).settle(key, Wasted::Evicted));
        }
        for kept in std::mem::take(&mut agent.kept) {
            self.settlements.push(kept.settle(key, Wasted::Evicted));
        }
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
        let generation = self.sessions[key].agents[agent_id].generation;
        self.discard(key, agent_id, reason, wasted);
        if let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        {
            agent.generation = generation;
        }
    }

    /// The rows the store answered, restored within the limits the file sets now rather than the ones the save ran
    /// under: sessions newest first, each with its main agent and then its other agents newest first, and the recipes
    /// of what was restored newest first while the retained bytes allow. What does not fit is disposed, its rows
    /// filed for deletion and each recipe among them counted as evicted. Ended sessions, rows this build cannot read
    /// and agents without a session row go too.
    fn restore_persisted(
        &mut self,
        (sessions, agents, counters): Loaded,
        now: i64,
        limits: &KeepaliveLimits,
        timing: KeepaliveTiming,
    ) {
        for row in counters {
            let counters = self.idle_returns.entry(row.pool_id).or_default();
            if let Some(bucket) = counters.buckets.get_mut(row.bucket as usize) {
                *bucket = IdleReturnBucket {
                    reached: row.reached,
                    returned: row.returned,
                };
            }
        }
        let mut by_session: BTreeMap<PersistKey, Vec<KeepaliveAgentRow>> = BTreeMap::new();
        for row in agents {
            by_session
                .entry((
                    row.key_id.clone(),
                    row.key_generation,
                    row.session_id.clone(),
                ))
                .or_default()
                .push(row);
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
        let mut unopened: Vec<(SessionKey, String)> = Vec::new();
        let mut held: Vec<(i64, SessionKey, String, Observation)> = Vec::new();
        for row in sessions {
            let stored: PersistKey = (
                row.key_id.clone(),
                row.key_generation,
                row.session_id.clone(),
            );
            let agent_rows = by_session.remove(&stored).unwrap_or_default();
            let payload = (row.record_version == RECORD_VERSION)
                .then(|| serde_json::from_str::<PersistedSession>(&row.payload).ok())
                .flatten();
            let Some(payload) = payload else {
                tracing::warn!(key = %row.key_id, session = %row.session_id, "a saved keepalive session could not be read and is dropped");
                self.file_removed_session(stored);
                continue;
            };
            let key: SessionKey = (row.key_id.clone(), row.session_id.clone());
            // An ended session is a tombstone for reports still in flight, and none of those survive the restart. A
            // second row under the same ids is an older generation's, which a newer one replaced.
            if payload.ended || self.sessions.contains_key(&key) {
                self.file_removed_session(stored);
                continue;
            }
            // Read before any limit, so a main agent this build cannot read counts as none.
            let mut agents: Vec<(KeepaliveAgentRow, Option<Decoded>)> = agent_rows
                .into_iter()
                .map(|agent_row| {
                    let decoded = decode_agent(&agent_row, timing);
                    if decoded.is_none() {
                        tracing::warn!(key = %row.key_id, session = %row.session_id, agent = %agent_row.agent_id, "a saved keepalive agent could not be read and is dropped");
                    }
                    (agent_row, decoded)
                })
                .collect();
            // Every report the mod sends names the main agent, so a session restored without one could never be
            // re-adopted.
            if !agents
                .iter()
                .any(|(agent_row, decoded)| agent_row.agent_id == MAIN_AGENT && decoded.is_some())
            {
                tracing::warn!(key = %row.key_id, session = %row.session_id, "a saved keepalive session without a readable main agent is dropped");
                self.file_removed_session(stored);
                continue;
            }
            let over = if self.sessions.len() >= limits.max_sessions {
                Some(&mut by_sessions)
            } else if self.agents >= limits.max_agents {
                Some(&mut by_agents)
            } else {
                None
            };
            if let Some(tally) = over {
                tally.sessions += 1;
                for (agent_row, decoded) in agents {
                    self.dispose_loaded(&key, &agent_row, decoded, tally);
                }
                self.file_removed_session(stored);
                continue;
            }
            agents.sort_by(|(a, _), (b, _)| {
                (b.agent_id == MAIN_AGENT)
                    .cmp(&(a.agent_id == MAIN_AGENT))
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
                    .then_with(|| a.agent_id.cmp(&b.agent_id))
            });
            let mut session = payload.restore();
            for (agent_row, decoded) in agents {
                let Some((agent, observation)) = decoded else {
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                };
                if self.agents >= limits.max_agents {
                    self.dispose_loaded(
                        &key,
                        &agent_row,
                        Some((agent, observation)),
                        &mut by_agents,
                    );
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                }
                self.next_incarnation = self.next_incarnation.max(agent.incarnation);
                let agent_id = agent_row.agent_id.clone();
                match (observation, agent_row.body.clone()) {
                    (None, _) => {}
                    (Some(observation), Some(body)) => {
                        match observation.restore(&agent_row, body) {
                            Some(observation) => held.push((
                                agent_row.updated_at,
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
                session.agents.insert(agent_id, agent);
                self.agents += 1;
            }
            self.sessions.insert(key, session);
        }
        // Agent rows whose session row is gone or was dropped above.
        for (stored, rows) in by_session {
            for row in rows {
                self.file_removed_agent(stored.clone(), row.agent_id);
            }
        }
        for (key, agent_id) in unopened {
            tracing::info!(key = %key.0, session = %key.1, agent = %agent_id, "a saved keepalive prompt could not be opened and is dropped");
            self.discard_at_load(&key, &agent_id, Reason::Reset, None);
        }
        held.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| (&a.1, &a.2).cmp(&(&b.1, &b.2))));
        for (_, key, agent_id, observation) in held {
            let size = observation.recipe.size;
            self.retained += size;
            let agent = self
                .sessions
                .get_mut(&key)
                .and_then(|s| s.agents.get_mut(&agent_id))
                .expect("the agent the load just restored");
            let expires = agent.expires;
            agent.current = Some(observation);
            if let Some(expires) = expires.filter(|expires| *expires <= now) {
                tracing::info!(key = %key.0, session = %key.1, agent = %agent_id, expires, "a keepalive recipe's entry lapsed while the router was down");
                self.discard_at_load(
                    &key,
                    &agent_id,
                    Reason::ExpiredAtRestart,
                    Some(Wasted::Expired),
                );
                continue;
            }
            if self.retained > limits.max_retained_bytes {
                by_bytes += 1;
                self.discard_at_load(&key, &agent_id, Reason::Evicted, Some(Wasted::Evicted));
            }
        }
        if by_sessions.sessions > 0 {
            tracing::info!(
                sessions = by_sessions.sessions,
                agents = by_sessions.agents,
                recipes = by_sessions.recipes,
                max_sessions = limits.max_sessions,
                "keepalive sessions past max_sessions were not restored"
            );
        }
        if by_agents.agents > 0 {
            tracing::info!(
                sessions = by_agents.sessions,
                agents = by_agents.agents,
                recipes = by_agents.recipes,
                max_agents = limits.max_agents,
                "keepalive agents past max_agents were not restored"
            );
        }
        if by_bytes > 0 {
            tracing::info!(
                recipes = by_bytes,
                max_retained_bytes = limits.max_retained_bytes,
                "keepalive recipes past max_retained_bytes were not restored"
            );
        }
    }
}

impl Engine {
    /// Reads back what the last process saved, at start and before the scheduler ticks, bounded by the current limits.
    /// The rows the load drops or rewrites are saved before it returns, so the store and the map agree from here.
    pub async fn load_persisted(self: &Arc<Self>) {
        if !self.lock().sessions.is_empty() {
            tracing::warn!("the keepalive's saved state is read once, at start, and this engine already holds sessions");
            return;
        }
        let store = self.parts.store.clone();
        let loaded = match tokio::task::spawn_blocking(move || store.load_keepalive()).await {
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
            if !inner.sessions.is_empty() {
                return;
            }
            inner.restore_persisted(loaded, now, &self.parts.limits, self.parts.timing);
        }
        // The settlements of the recipes the load ended.
        self.flush();
        self.flush_persisted().await;
    }

    /// A snapshot, its save and its acknowledgment, on the blocking pool under the flush lock whole: two flushes never
    /// overlap, an older batch never follows a newer one to the store, and a flush whose caller went away still runs
    /// to its end, so the shutdown's flush queues behind it. A failed save keeps every row dirty for the next flush.
    pub async fn flush_persisted(self: &Arc<Self>) {
        let engine = self.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || engine.flush_blocking()).await {
            tracing::warn!(%error, "the keepalive's flush did not finish");
        }
    }

    fn flush_blocking(&self) {
        self.persist_entries.fetch_add(1, Ordering::SeqCst);
        self.persist_progress.notify_waiters();
        // A panic inside a flush poisons the lock, which orders flushes and guards no invariant of its own.
        let _flushing = self
            .flush_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = self.now();
        let snapshot = self.lock().persist_snapshot(now);
        self.persist_snapshots.fetch_add(1, Ordering::SeqCst);
        self.persist_progress.notify_waiters();
        if snapshot.is_empty() {
            return;
        }
        if snapshot.batch.is_empty() {
            self.lock().acknowledge(&snapshot);
            return;
        }
        match self.parts.store.save_keepalive(&snapshot.batch) {
            Ok(()) => {
                let mut inner = self.lock();
                inner.acknowledge(&snapshot);
                if std::mem::take(&mut inner.persist_failing) {
                    tracing::info!("the keepalive's state reaches the registry again");
                }
            }
            Err(error) => {
                let mut inner = self.lock();
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

    /// The shutdown's first step: the scheduler, the flusher and the probes in flight are cancelled, and the intake
    /// closes, so a request's event or a report that lands later is refused and counted rather than applied after the
    /// last save. Taking the lock is the drain: an update that already holds it finishes before the flag is set.
    pub fn quiesce(&self) {
        self.cancel.cancel();
        self.lock().closed = true;
    }

    /// How many flushes have read their batch. Compiled only under `debug-hooks`, as `router_views` is.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn persist_snapshots(&self) -> u64 {
        self.persist_snapshots.load(Ordering::SeqCst)
    }

    /// How many flushes have entered, counted before the flush lock. Compiled only under `debug-hooks`.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn persist_entries(&self) -> u64 {
        self.persist_entries.load(Ordering::SeqCst)
    }

    /// Resolves once `entries` flushes have entered and `snapshots` have read their batch. A flush raises the counts
    /// on its own thread, so this needs no timer, which a runtime whose workers wait behind a held save would not
    /// drive. Compiled only under `debug-hooks`.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub async fn persist_reached(&self, entries: u64, snapshots: u64) {
        loop {
            let notified = self.persist_progress.notified();
            tokio::pin!(notified);
            // Registered before the counts are read, so a flush that raises one between the two wakes this waiter.
            notified.as_mut().enable();
            if self.persist_entries() >= entries && self.persist_snapshots() >= snapshots {
                return;
            }
            notified.await;
        }
    }

    /// Every session and agent as the next save would write it, dirty or not, for a test that holds what a restart
    /// restored against what was saved. Compiled only under `debug-hooks`.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn persist_rows(&self) -> (Vec<KeepaliveSessionRow>, Vec<KeepaliveAgentRow>) {
        let now = self.now();
        let inner = self.lock();
        let mut sessions = Vec::new();
        let mut agents = Vec::new();
        for session in inner.sessions.values() {
            let row: PersistKey = (
                session.key_id.clone(),
                session.key_generation,
                session.session_id.clone(),
            );
            sessions.push(session_row(&row, session, now).expect("a session payload serializes"));
            for agent in session.agents.values() {
                agents.push(agent_row(&row, agent, now).expect("an agent payload serializes"));
            }
        }
        (sessions, agents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The float a restart round trip once moved by one unit in the last place, through the default parser.
    const DRIFTS: f64 = 0.9445756495771439;

    #[test]
    fn a_float_in_the_payload_reads_back_to_the_bits_it_was_saved_with() {
        #[derive(Serialize, Deserialize)]
        struct Jitter(#[serde(with = "exact")] f64);
        for value in [DRIFTS, -DRIFTS] {
            let back: Jitter =
                serde_json::from_str(&serde_json::to_string(&Jitter(value)).unwrap()).unwrap();
            assert_eq!(back.0.to_bits(), value.to_bits());
        }
        let kept = PersistedKept {
            key_generation: 1,
            pool_id: "main".into(),
            canonical_model: "m".into(),
            account: None,
            fast: false,
            costs: vec![DRIFTS, f64::MIN_POSITIVE],
            entry_expires: 0,
        };
        let back: PersistedKept =
            serde_json::from_str(&serde_json::to_string(&kept).unwrap()).unwrap();
        assert_eq!(
            back.costs.iter().map(|c| c.to_bits()).collect::<Vec<_>>(),
            kept.costs.iter().map(|c| c.to_bits()).collect::<Vec<_>>()
        );
    }
}
