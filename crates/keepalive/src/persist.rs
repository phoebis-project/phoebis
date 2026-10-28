//! The engine's state across a restart: its sessions, its agents with their retained prompts, and the idle-age
//! counters, written behind the map to the registry and read back once at start. The map stays the truth while the
//! engine runs, and the store is what the next process starts from. The relay's session memory has the same shape: a
//! dirty set under revisions, one flush at a time on the blocking pool, and an acknowledgment that retires only what
//! the save carried.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use phoebis_ledger::{Band, Speed};
use phoebis_store::keepalive::BodyWrite;
use phoebis_store::{IdleReturnsRow, KeepaliveAgentRow, KeepaliveBatch, KeepaliveSessionRow};
use serde::{Deserialize, Serialize};

use crate::config::{KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, LEASE_MS};
use crate::engine::{
    kind_of_word, kind_word, Agent, AgentState, Engine, Guard, IdleEpisode, IdleReturnBucket,
    Inner, Kept, Observation, ParkKind, PrefixAnchor, PriorCoverage, Reason, SavingsUnit, Session,
    SessionKey, Settlement, UnansweredProbe, Wasted, MAIN_AGENT,
};
use crate::recipe::Recipe;
use crate::usage::ProbeUsage;

/// The payloads' shape. A row of an older version is dropped at the load: version 1 carried no funding and version 2
/// a dollar in place of the probe's usage, tier, instant and band, so neither settles under this build's rule.
pub(crate) const RECORD_VERSION: u32 = 3;

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
    pub last_prefix: Option<PersistedAnchor>,
    /// Absent from a row saved before agents carried one, which loads as `None`, so the watch marks nothing until the
    /// next main completion reports a prompt.
    #[serde(default)]
    pub last_prompt_tokens: Option<u64>,
    pub generation: u64,
    pub current: Option<PersistedObservation>,
    pub expires: Option<i64>,
    /// When the released record leaves the engine, as `Agent.leaves_at` carries it, `None` on a record that is not
    /// released.
    pub leaves_at: Option<i64>,
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
    /// Absent from a row saved before agents carried one, which loads as `None`, the base regime, until the next
    /// request names one.
    #[serde(default)]
    pub entrypoint: Option<String>,
    pub reason: String,
    pub last_guard: Option<Guard>,
    pub hold_counted: bool,
    pub paused: bool,
    pub probes: u64,
    pub migrations: u64,
    pub usage: PersistedUsage,
    pub unanswered: Vec<PersistedUnanswered>,
    pub kept: Vec<PersistedKept>,
    pub last_real_at: Option<i64>,
    /// Absent from a row an earlier build saved, which loads as `None`, so a restart keeps that row's recipe and the
    /// next successful completion sets it.
    #[serde(default)]
    pub last_answered_at: Option<i64>,
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
    pub anchor: Option<PersistedAnchor>,
    pub prompt_at: Option<i64>,
    pub pool_key: String,
    /// Absent from a row saved before windows carried one, which loads as `None`. That window's reach was counted
    /// before the counters were kept by day, so it is held under no day, and its return counts nothing unless the
    /// request that ends it raises a bucket.
    #[serde(default)]
    pub bucket_day: Option<i64>,
}

/// Saved as `[static_hash, last, before_last]`, read from that or from the saved `[static_hash, last]` pair, so a row
/// an earlier build saved loads under the same record version with no boundary before the last.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum PersistedAnchor {
    Anchor(String, String, Option<String>),
    Tail(String, String),
}

impl PersistedAnchor {
    fn of(anchor: &PrefixAnchor) -> PersistedAnchor {
        PersistedAnchor::Anchor(
            anchor.static_hash.clone(),
            anchor.last.clone(),
            anchor.before_last.clone(),
        )
    }

    fn restore(self) -> PrefixAnchor {
        match self {
            PersistedAnchor::Anchor(static_hash, last, before_last) => PrefixAnchor {
                static_hash,
                last,
                before_last,
            },
            PersistedAnchor::Tail(static_hash, last) => PrefixAnchor {
                static_hash,
                last,
                before_last: None,
            },
        }
    }
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
    pub kind: Option<String>,
    /// Absent from a payload written before the class rode the observation, which reads false and settles as before.
    #[serde(default)]
    pub usage_based: bool,
    pub in_overage: bool,
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
pub(crate) struct PersistedUnanswered {
    pub id: String,
    pub usage: PersistedUsage,
    pub tier: Option<String>,
    pub at: i64,
    /// `Band::as_str`'s word.
    pub band: String,
    pub unit: Option<String>,
    pub account: Option<String>,
    pub fast: bool,
    pub model: String,
    pub expires: i64,
    pub covered: bool,
    pub kind: Option<String>,
    /// Absent from a payload written before the class rode the probe, which reads false and settles as before.
    #[serde(default)]
    pub usage_based: bool,
    pub in_overage: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedKept {
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub account: Option<String>,
    pub fast: bool,
    pub usage: PersistedUsage,
    pub tier: Option<String>,
    pub at: i64,
    /// `Band::as_str`'s word.
    pub band: String,
    pub entry_expires: i64,
    pub kind: Option<String>,
    /// Absent from a payload written before the class rode the probe, which reads false and settles as before.
    #[serde(default)]
    pub usage_based: bool,
    pub in_overage: bool,
    pub covered: bool,
    pub unit: Option<String>,
    pub pending: Option<String>,
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

fn persisted_usage(usage: &ProbeUsage) -> PersistedUsage {
    PersistedUsage {
        read: usage.read,
        write: usage.write,
        output: usage.output,
        five_minutes: usage.five_minutes,
        one_hour: usage.one_hour,
        input: usage.input,
        speed: usage.speed.map(|speed| {
            match speed {
                Speed::Fast => "fast",
                Speed::Standard => "standard",
            }
            .to_string()
        }),
    }
}

fn probe_usage(saved: PersistedUsage) -> ProbeUsage {
    ProbeUsage {
        read: saved.read,
        write: saved.write,
        output: saved.output,
        five_minutes: saved.five_minutes,
        one_hour: saved.one_hour,
        input: saved.input,
        speed: saved.speed.as_deref().map(|word| Speed::of(Some(word))),
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

/// A word the payload carries for an optional account kind: `None` reads as none, and a word this build does not know
/// refuses the row.
fn optional_kind(word: Option<String>) -> Option<Option<phoebis_store::AccountKind>> {
    match word {
        None => Some(None),
        Some(word) => kind_of_word(&word).map(Some),
    }
}

/// A word the payload carries for an optional wasted label: `None` reads as none, and a word this build does not know
/// refuses the row.
fn optional_wasted(word: Option<String>) -> Option<Option<Wasted>> {
    match word {
        None => Some(None),
        Some(word) => Wasted::ALL
            .iter()
            .copied()
            .find(|label| label.as_str() == word)
            .map(Some),
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
    /// registration is the one the mod would send next. A session saved attended keeps the attended reading of the
    /// ceilings for one lease length from `now`, so the ticks before that registration discard no park as idle.
    fn restore(self, now: i64) -> Session {
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
            restored_until: self.attended.then_some(now + LEASE_MS),
            owner_mismatches: 0,
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
                anchor: episode.anchor.as_ref().map(PersistedAnchor::of),
                prompt_at: episode.prompt_at,
                pool_key: episode.pool_key.clone(),
                bucket_day: episode.bucket_day,
            }),
            idle_window_floor: agent.idle_window_floor,
            last_prefix: agent.last_prefix.as_ref().map(PersistedAnchor::of),
            last_prompt_tokens: agent.last_prompt_tokens,
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
                kind: o.kind.map(|kind| kind_word(kind).to_string()),
                usage_based: o.usage_based,
                in_overage: o.in_overage,
            }),
            expires: agent.expires,
            leaves_at: agent.leaves_at,
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
            entrypoint: agent.entrypoint.clone(),
            reason: agent.reason.as_str().to_string(),
            last_guard: agent.last_guard,
            hold_counted: agent.hold_counted,
            paused: agent.paused,
            probes: agent.probes,
            migrations: agent.migrations,
            usage: persisted_usage(&agent.usage),
            unanswered: agent
                .unanswered
                .iter()
                .map(|probe| PersistedUnanswered {
                    id: probe.id.clone(),
                    usage: persisted_usage(&probe.usage),
                    tier: probe.tier.clone(),
                    at: probe.at,
                    band: probe.band.as_str().to_string(),
                    unit: probe.unit.map(unit_word),
                    account: probe.account.clone(),
                    fast: probe.fast,
                    model: probe.model.clone(),
                    expires: probe.expires,
                    covered: probe.covered,
                    kind: probe.kind.map(|kind| kind_word(kind).to_string()),
                    usage_based: probe.usage_based,
                    in_overage: probe.in_overage,
                })
                .collect(),
            kept: agent
                .kept
                .iter()
                .map(|kept| PersistedKept {
                    key_generation: kept.key_generation,
                    pool_id: kept.pool_id.clone(),
                    canonical_model: kept.canonical_model.clone(),
                    account: kept.account.clone(),
                    fast: kept.fast,
                    usage: persisted_usage(&kept.usage),
                    tier: kept.tier.clone(),
                    at: kept.at,
                    band: kept.band.as_str().to_string(),
                    entry_expires: kept.entry_expires,
                    kind: kept.kind.map(|kind| kind_word(kind).to_string()),
                    usage_based: kept.usage_based,
                    in_overage: kept.in_overage,
                    covered: kept.covered,
                    unit: kept.unit.map(unit_word),
                    pending: kept.pending.map(|label| label.as_str().to_string()),
                })
                .collect(),
            last_real_at: agent.last_real_at,
            last_answered_at: agent.last_answered_at,
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
            anchor: episode.anchor.map(PersistedAnchor::restore),
            prompt_at: episode.prompt_at,
            pool_key: episode.pool_key,
            bucket_day: episode.bucket_day,
        });
        agent.idle_window_floor = self.idle_window_floor;
        agent.last_prefix = self.last_prefix.map(PersistedAnchor::restore);
        agent.last_prompt_tokens = self.last_prompt_tokens;
        agent.generation = self.generation;
        agent.expires = self.expires;
        agent.leaves_at = self.leaves_at;
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
        agent.entrypoint = self.entrypoint;
        agent.hold_counted = self.hold_counted;
        agent.paused = self.paused;
        agent.probes = self.probes;
        agent.migrations = self.migrations;
        agent.usage = probe_usage(self.usage);
        agent.unanswered = self
            .unanswered
            .into_iter()
            .map(|probe| {
                Some(UnansweredProbe {
                    id: probe.id,
                    usage: probe_usage(probe.usage),
                    tier: probe.tier,
                    at: probe.at,
                    band: Band::of_rollup(&probe.band),
                    unit: optional_unit(probe.unit)?,
                    account: probe.account,
                    fast: probe.fast,
                    model: probe.model,
                    expires: probe.expires,
                    covered: probe.covered,
                    kind: optional_kind(probe.kind)?,
                    usage_based: probe.usage_based,
                    in_overage: probe.in_overage,
                })
            })
            .collect::<Option<_>>()?;
        agent.kept = self
            .kept
            .into_iter()
            .map(|kept| {
                Some(Kept {
                    key_generation: kept.key_generation,
                    pool_id: kept.pool_id,
                    canonical_model: kept.canonical_model,
                    account: kept.account,
                    fast: kept.fast,
                    usage: probe_usage(kept.usage),
                    tier: kept.tier,
                    at: kept.at,
                    band: Band::of_rollup(&kept.band),
                    entry_expires: kept.entry_expires,
                    kind: optional_kind(kept.kind)?,
                    usage_based: kept.usage_based,
                    in_overage: kept.in_overage,
                    covered: kept.covered,
                    unit: optional_unit(kept.unit)?,
                    pending: optional_wasted(kept.pending)?,
                })
            })
            .collect::<Option<_>>()?;
        agent.last_real_at = self.last_real_at;
        agent.last_answered_at = self.last_answered_at;
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
            // The request it followed ended with the process, so no credit reads its receipt.
            received_at: self.started,
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
            kind: optional_kind(self.kind)?,
            usage_based: self.usage_based,
            in_overage: self.in_overage,
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

/// One agent row as the store keeps it: the payload, the recipe's columns when one is retained, and its prompt only
/// when the recipe changed since the last landed save that carried it.
fn agent_row(
    key: &PersistKey,
    agent: &Agent,
    now: i64,
) -> Result<KeepaliveAgentRow, serde_json::Error> {
    let recipe = agent.current.as_ref().map(|o| &o.recipe);
    let no_headers: &[(String, String)] = &[];
    let body_write = match recipe {
        Some(recipe) if agent.body_dirty => BodyWrite::Seal(recipe.body.clone()),
        None if agent.body_saved => BodyWrite::Clear,
        _ => BodyWrite::Keep,
    };
    Ok(KeepaliveAgentRow {
        key_id: key.0.clone(),
        key_generation: key.1,
        session_id: key.2.clone(),
        agent_id: agent.id.clone(),
        record_version: RECORD_VERSION,
        payload: serde_json::to_string(&PersistedAgent::of(agent))?,
        body: None,
        body_write,
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
    /// The settlements the load held for a save, taken under the lock this snapshot was read under: a save that lands
    /// releases them, and one that fails puts them back in front of whatever the load queued since.
    pub settled: Vec<Settlement>,
    sessions: Vec<(PersistKey, u64)>,
    agents: Vec<((PersistKey, String), u64)>,
    /// The agent rows whose body this save seals (true) or clears (false), at the revision each was read at.
    bodies: Vec<((PersistKey, String), u64, bool)>,
    removed_sessions: Vec<(PersistKey, u64)>,
    removed_agents: Vec<((PersistKey, String), u64)>,
    pools: Vec<(String, u64)>,
    /// The engine's recorded cutoff as this snapshot read it, content of its own: a roll that dropped a day while no
    /// pool was dirty still has the registry's rows of that day to delete.
    purge_counters_before: Option<i64>,
}

impl Snapshot {
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
            && self.agents.is_empty()
            && self.removed_sessions.is_empty()
            && self.removed_agents.is_empty()
            && self.pools.is_empty()
            && self.purge_counters_before.is_none()
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

    /// The batch the dirty sets name, read as the map stands now, each row with the revision it was read at, and the
    /// settlements the load held for a save. No dirty entry is cleared here: `acknowledge` does that once the save has
    /// landed. A dirty row whose record left the map without a removal, which only `close` does, is carried by nothing
    /// and retired with the rest.
    pub(crate) fn persist_snapshot(&mut self, now: i64) -> Snapshot {
        let mut snapshot = Snapshot {
            settled: std::mem::take(&mut self.settlements_after_save),
            ..Snapshot::default()
        };
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
                    Ok(agent_row) => {
                        let sealed = match agent_row.body_write {
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
                        snapshot.batch.agents.push(agent_row);
                    }
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
            for (day, counters) in self.idle_returns.get(pool_id).into_iter().flatten() {
                for (bucket, counter) in counters.buckets.iter().enumerate() {
                    snapshot.batch.counters.push(IdleReturnsRow {
                        pool_id: pool_id.clone(),
                        day: *day,
                        bucket: bucket as u32,
                        reached: counter.reached,
                        returned: counter.returned,
                    });
                }
            }
            snapshot.pools.push((pool_id.clone(), *revision));
        }
        snapshot.purge_counters_before = self.purge_counters_before;
        snapshot.batch.purge_counters_before = self.purge_counters_before;
        snapshot
    }

    /// The settlements a landed save carried go to the ledger's queue, which the flush drains once the lock is
    /// released.
    pub(crate) fn release_settled(&mut self, settled: Vec<Settlement>) {
        self.settlements.extend(settled);
    }

    /// Retires what a landed save carried: an entry leaves only while its revision is the one saved, so a change
    /// since the snapshot keeps it dirty for the next flush. The body the save sealed or cleared is the registry's
    /// whatever moved since, while the mark asking for a seal leaves only with its row's entry, since a recipe set
    /// after the snapshot is a body this save did not carry.
    pub(crate) fn acknowledge(&mut self, snapshot: &Snapshot) {
        for ((row, agent_id), revision, sealed) in &snapshot.bodies {
            let unchanged =
                self.dirty.agents.get(&(row.clone(), agent_id.clone())) == Some(revision);
            if let Some(agent) = self
                .sessions
                .get_mut(&(row.0.clone(), row.2.clone()))
                .filter(|session| session.key_generation == row.1)
                .and_then(|session| session.agents.get_mut(agent_id))
            {
                agent.body_saved = *sealed;
                if unchanged {
                    agent.body_dirty = false;
                }
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
        // Cleared only while unchanged since the snapshot: a roll in between recorded a later cutoff, which the next
        // save still owes.
        if self.purge_counters_before == snapshot.purge_counters_before {
            self.purge_counters_before = None;
        }
    }

    /// An agent row a limit leaves out of the load. Its recipe counts as evicted, and the probes it left unanswered and
    /// the ones a request kept settle under `evicted`, as they do when the retained bytes evict a recipe through
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
        for probe in std::mem::take(&mut agent.unanswered) {
            self.settlements
                .push(Kept::of(&agent, &probe).settle(key, Wasted::Evicted));
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
    /// filed for deletion and each recipe among them counted as evicted. Agents without a session row, ended sessions,
    /// released records whose leave has passed, rows of an older version and rows this build cannot read go too.
    fn restore_persisted(
        &mut self,
        (sessions, agents, counters): Loaded,
        now: i64,
        limits: &KeepaliveLimits,
        timing: KeepaliveTiming,
    ) {
        for row in counters {
            let counters = self
                .idle_returns
                .entry(row.pool_id)
                .or_default()
                .entry(row.day)
                .or_default();
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
        let mut dropped = 0usize;
        let mut unopened: Vec<(SessionKey, String)> = Vec::new();
        let mut held: Vec<(i64, SessionKey, String, Observation)> = Vec::new();
        for row in sessions {
            let stored: PersistKey = (
                row.key_id.clone(),
                row.key_generation,
                row.session_id.clone(),
            );
            let agent_rows = by_session.remove(&stored).unwrap_or_default();
            // Counted for one line after the loop rather than one per row: every row an upgrade finds is one of them.
            if row.record_version < RECORD_VERSION {
                dropped += 1;
                for agent_row in agent_rows {
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                }
                self.file_removed_session(stored);
                continue;
            }
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
            let mut session = payload.restore(now);
            for (agent_row, decoded) in agents {
                let Some((mut agent, observation)) = decoded else {
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                };
                // The sweep has not run yet at the load, and a leave that passed while the router was down is already
                // due, so the record goes here, before the cap read below can give it a live agent's slot.
                if agent.state == AgentState::Released
                    && agent.leaves_at.is_some_and(|at| at <= now)
                {
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                }
                // Read off the row rather than the opened body, so a blob that no longer opens is cleared by the
                // agent's next save.
                agent.body_saved = agent_row.body_size > 0;
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
                tracing::info!(
                    key = %key.0,
                    session = %key.1,
                    agent = %agent_id,
                    expires = %phoebis_ledger::utc_stamp(expires),
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
            if self.retained > limits.max_retained_bytes {
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
            // The registry first, then the ledger: the settlements wait for the save that carries each ended recipe's
            // agent without the probes they settle, so a failed save sends nothing and a crash between the save and
            // the ledger's write leaves a wasted probe unwritten rather than written again by the next load.
            let settled = std::mem::take(&mut inner.settlements);
            inner.settlements_after_save.extend(settled);
        }
        self.flush_persisted().await;
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

    fn flush_blocking(&self) {
        self.persist_entries.fetch_add(1, Ordering::SeqCst);
        self.persist_progress.notify_waiters();
        // A panic inside a flush poisons the lock, which orders flushes and guards no invariant of its own.
        let _flushing = self
            .flush_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = self.now();
        let mut snapshot = self.lock().persist_snapshot(now);
        self.persist_snapshots.fetch_add(1, Ordering::SeqCst);
        self.persist_progress.notify_waiters();
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
        match self.parts.store.save_keepalive(&snapshot.batch) {
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
                let queued = std::mem::replace(&mut inner.settlements_after_save, settled);
                inner.settlements_after_save.extend(queued);
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

    /// Every session and agent as the next save would write it, dirty or not, with each agent's prompt and body as a
    /// load reads them back, for a test that holds what a restart restored against what was saved. Compiled only under
    /// `debug-hooks`.
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
                agents.push(KeepaliveAgentRow {
                    body: agent.current.as_ref().map(|o| o.recipe.body.clone()),
                    body_write: BodyWrite::Keep,
                    ..agent_row(&row, agent, now).expect("an agent payload serializes")
                });
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
        for value in [DRIFTS, -DRIFTS, f64::MIN_POSITIVE] {
            let back: Jitter =
                serde_json::from_str(&serde_json::to_string(&Jitter(value)).unwrap()).unwrap();
            assert_eq!(back.0.to_bits(), value.to_bits());
        }
    }

    #[test]
    fn a_persisted_agent_without_an_entrypoint_loads_with_none() {
        let mut agent = Agent::new("main", "k", 1, 0, KeepaliveTiming::default());
        agent.entrypoint = Some("cli".into());
        let payload = serde_json::to_value(PersistedAgent::of(&agent)).unwrap();
        let saved: PersistedAgent = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(
            saved.entrypoint.as_deref(),
            Some("cli"),
            "the agent's entrypoint is saved with it"
        );
        let mut before = payload;
        before
            .as_object_mut()
            .expect("the payload is an object")
            .remove("entrypoint");
        let loaded: PersistedAgent =
            serde_json::from_value(before).expect("a payload saved before the member loads");
        assert_eq!(loaded.entrypoint, None);
        let (restored, _) = loaded
            .restore(KeepaliveTiming::default())
            .expect("every word of the payload is known");
        assert_eq!(
            restored.entrypoint, None,
            "its probes price at the base regime until the next request names one"
        );
    }
}
