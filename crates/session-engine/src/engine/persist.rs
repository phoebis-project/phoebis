//! The engine's state across a restart: its sessions, its agents with their retained prompts, and the idle-age
//! counters, written behind the map to the registry and read back once at start. The map stays the truth while the
//! engine runs, and the store is what the next process starts from. The relay's session memory has the same shape: a
//! dirty set under revisions, one flush at a time on the blocking pool, and an acknowledgment that retires only what
//! the save carried. The facts that a companion reports go to the registry the same way, each set as the report filed
//! it, and the save that a filing asks for runs at once.

use std::collections::BTreeMap;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use phoebis_ledger::{Band, Speed};
use phoebis_routing::{RegistrySnapshot, FIVE_HOUR, SEVEN_DAY};
use phoebis_store::keepalive::BodyWrite;
use phoebis_store::{
    FactsKey, FactsKind, FactsRow, IdleReturnsRow, KeepaliveAgentRow, KeepaliveBatch,
    KeepaliveSessionRow,
};
use serde::{Deserialize, Serialize};

use crate::engine::config::KeepaliveLimits;
use crate::engine::{Engine, Inner};
use crate::sessions::config::LEASE_MS;
use crate::sessions::facts::{
    AgentFacts, RawAgentFacts, RawSessionFacts, SessionFacts, StoredSessionFacts,
};
use crate::sessions::state::{
    agent_ref, Agent, AgentState, HeldPark, Park, ParkKind, Session, SessionKey, AGENT_HELD,
    MAIN_AGENT,
};
use crate::warming::agent::{warming_mut, warming_ref, AgentWarming};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::KeepaliveTiming;
use crate::warming::guard::Guard;
use crate::warming::idle::{IdleEpisode, IdleReturnBucket, PrefixAnchor};
use crate::warming::observation::{
    Kept, Observation, PriorCoverage, ProbeCharge, Settlement, UnansweredProbe,
};
use crate::warming::recipe::Recipe;
use crate::warming::savings::{AttemptFunding, RowClient, SavingsUnit};
use crate::warming::state::{Change, Changes};
use crate::warming::usage::ProbeUsage;

/// The payloads' shape. A row of an older version is dropped at the load: version 1 carried no funding and version 2
/// a dollar in place of the probe's usage, tier, instant and band, so neither settles under this build's rule.
pub(crate) const RECORD_VERSION: u32 = 3;

/// The shape of a stored set's value: a JSON object of the members of the row's kind under the raw form's names, an
/// absent one as `null`.
pub(crate) const FACTS_RECORD_VERSION: u32 = 1;

/// A session row's key in the store: key id, key generation, session id.
pub(crate) type PersistKey = (String, i64, String);

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

/// An agent's saved members, from its `Agent` and its `AgentWarming`, with the observation's fields and the recipe's
/// own beside them. The prompt, its header block, its size, its speed and the dispatch TTL are the row's columns, so
/// the body is sealed apart from the payload.
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
    /// `last_prefix`'s marked boundary. Absent from a row saved before anchors carried one, which loads as `None`, so
    /// the anchor holds its last two boundaries alone until main's next request.
    #[serde(default)]
    pub last_prefix_marked: Option<String>,
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
    pub last_activity: i64,
    pub cache_started: i64,
    pub ttl_ms: i64,
    pub cold: u32,
    pub failures: u32,
    pub account: Option<String>,
    /// The incarnation of `account` when the armed recipe was last answered on it. Absent from a row saved before
    /// agents carried one, which loads as `None`, so the recipe holds under `pin_moved` until the agent's next request
    /// arms it.
    #[serde(default)]
    pub account_incarnation: Option<u64>,
    pub pool_id: String,
    pub canonical_model: String,
    /// Absent from a row saved before agents carried one, which loads as `None`, the base regime, until the next
    /// request names one.
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Absent from a row saved before agents carried one, which loads as `None`, so the recipe's probes and skips key
    /// the reserved client until the next admitted request sets it.
    #[serde(default)]
    pub client_label: Option<String>,
    pub reason: Reason,
    pub last_guard: Option<Guard>,
    pub hold_counted: bool,
    pub paused: bool,
    pub probes: u64,
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
    /// `anchor`'s marked boundary, absent and loaded as `None` as `PersistedAgent.last_prefix_marked` is, so the
    /// window judges its return on the last two boundaries alone.
    #[serde(default)]
    pub anchor_marked: Option<String>,
    pub prompt_at: Option<i64>,
    pub pool_key: String,
    /// Absent from a row saved before windows carried one, which loads as `None`. That window's reach was counted
    /// before the counters were kept by day, so it is held under no day, and its return counts nothing unless the
    /// request that ends it raises a bucket.
    #[serde(default)]
    pub bucket_day: Option<i64>,
}

/// Saved as `[static_hash, last, before_last]`, read from that or from the saved `[static_hash, last]` pair, so a row
/// an earlier build saved loads under the same record version with no boundary before the last. The marked boundary
/// sits beside the array in a field of the host's own, so the array keeps the two shapes a binary of the same record
/// version reads.
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

    fn restore(self, marked: Option<String>) -> PrefixAnchor {
        match self {
            PersistedAnchor::Anchor(static_hash, last, before_last) => PrefixAnchor {
                static_hash,
                last,
                before_last,
                marked,
            },
            PersistedAnchor::Tail(static_hash, last) => PrefixAnchor {
                static_hash,
                last,
                before_last: None,
                marked,
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
    /// Absent from a payload the earlier build wrote, which priced in the 5-hour window and reads so.
    #[serde(default)]
    pub window: Option<String>,
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
    /// `Speed::as_str`'s word. Null in a payload an earlier build saved for an answer that named no speed, which reads
    /// as standard.
    pub speed: Option<String>,
}

/// A probe's charge, flattened into its unanswered and kept rows so a saved row keeps the member names it was written
/// with.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedCharge {
    pub account: Option<String>,
    pub at: i64,
    /// `Band::as_str`'s word.
    pub band: String,
    /// Absent from a payload written before the client and the user agent rode the probe, each read as `None`, which
    /// settles under the reserved one.
    #[serde(flatten)]
    pub client: RowClient,
    pub in_overage: bool,
    pub kind: Option<phoebis_store::AccountKind>,
    pub tier: Option<String>,
    pub usage: PersistedUsage,
    /// Absent from a payload written before the class rode the probe, which reads false and settles as before.
    #[serde(default)]
    pub usage_based: bool,
}

impl PersistedCharge {
    fn of(charge: &ProbeCharge) -> PersistedCharge {
        PersistedCharge {
            account: charge.account.clone(),
            at: charge.at,
            band: charge.band.as_str().to_string(),
            client: charge.client.clone(),
            in_overage: charge.funding.in_overage,
            kind: charge.funding.kind,
            tier: charge.tier.clone(),
            usage: persisted_usage(&charge.usage),
            usage_based: charge.funding.usage_based,
        }
    }

    fn restore(self) -> ProbeCharge {
        ProbeCharge {
            account: self.account,
            at: self.at,
            band: Band::of_rollup(&self.band),
            client: self.client,
            funding: AttemptFunding {
                kind: self.kind,
                usage_based: self.usage_based,
                in_overage: self.in_overage,
            },
            tier: self.tier,
            usage: probe_usage(self.usage),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedUnanswered {
    pub id: String,
    #[serde(flatten)]
    pub charge: PersistedCharge,
    pub unit: Option<SavingsUnit>,
    /// Absent from a payload the earlier build wrote, which priced in the 5-hour window and reads so.
    #[serde(default)]
    pub window: Option<String>,
    pub fast: bool,
    pub model: String,
    pub expires: i64,
    pub covered: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedKept {
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    #[serde(flatten)]
    pub charge: PersistedCharge,
    pub fast: bool,
    pub entry_expires: i64,
    pub covered: bool,
    pub unit: Option<SavingsUnit>,
    /// Absent from a payload the earlier build wrote, which priced in the 5-hour window and reads so.
    #[serde(default)]
    pub window: Option<String>,
    pub pending: Option<Wasted>,
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
        speed: Some(usage.speed.as_str().to_string()),
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
        speed: Speed::of(saved.speed.as_deref()),
    }
}

fn unit_word(unit: SavingsUnit) -> String {
    unit.as_str().to_string()
}

fn unit_of(word: &str) -> Option<SavingsUnit> {
    [SavingsUnit::Usd, SavingsUnit::WindowShare]
        .into_iter()
        .find(|unit| unit.as_str() == word)
}

/// A word the payload carries for an optional value: `None` reads as none, and a word `parse` does not know refuses
/// the row.
fn optional<T>(word: Option<String>, parse: impl FnOnce(&str) -> Option<T>) -> Option<Option<T>> {
    match word {
        None => Some(None),
        Some(word) => parse(&word).map(Some),
    }
}

/// The window a settlement's unit was priced in. `None` is an entry the earlier build saved, which priced in the
/// 5-hour window, so its settlement keeps that word, and a word this build does not know refuses the row.
fn window_of(window: Option<String>) -> Option<&'static str> {
    match window.as_deref() {
        None | Some(FIVE_HOUR) => Some(FIVE_HOUR),
        Some(SEVEN_DAY) => Some(SEVEN_DAY),
        Some(_) => None,
    }
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
    fn restore(self, now: i64) -> Session {
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
    fn of(agent: &Agent, warming: &AgentWarming) -> PersistedAgent {
        let (deadline, park_kind, wake_at) = Park::parts(agent.park.map(|held| held.park));
        PersistedAgent {
            id: agent.id.clone(),
            key_id: agent.key_id.clone(),
            key_generation: warming.key_generation,
            incarnation: agent.incarnation,
            state: agent.state,
            deadline,
            park_kind,
            wake_at,
            park_began: agent.park.map(|held| held.began),
            park_ended_at: agent.park_ended_at,
            idle_episode: warming
                .idle_episode
                .as_ref()
                .map(|episode| PersistedEpisode {
                    started: episode.started,
                    bucket: episode.bucket,
                    anchor: episode.anchor.as_ref().map(PersistedAnchor::of),
                    anchor_marked: episode.anchor.as_ref().and_then(|a| a.marked.clone()),
                    prompt_at: episode.prompt_at,
                    pool_key: episode.pool_key.clone(),
                    bucket_day: episode.bucket_day,
                }),
            idle_window_floor: warming.idle_window_floor,
            last_prefix: warming.last_prefix.as_ref().map(PersistedAnchor::of),
            last_prefix_marked: warming.last_prefix.as_ref().and_then(|a| a.marked.clone()),
            last_prompt_tokens: warming.last_prompt_tokens,
            generation: agent.generation,
            current: warming.current.as_ref().map(|o| PersistedObservation {
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
                    window: Some(prior.window.to_string()),
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
                kind: o.funding.kind.map(|kind| kind.as_str().to_string()),
                usage_based: o.funding.usage_based,
                in_overage: o.funding.in_overage,
            }),
            expires: warming.expires,
            leaves_at: agent.leaves_at,
            jitter_frac: warming.jitter_frac,
            retry_at: warming.retry_at,
            last_activity: agent.last_activity,
            cache_started: warming.cache_started,
            ttl_ms: warming.ttl_ms,
            cold: warming.cold,
            failures: warming.failures,
            account: warming.account.clone(),
            account_incarnation: warming.account_incarnation,
            pool_id: warming.pool_id.clone(),
            canonical_model: warming.canonical_model.clone(),
            entrypoint: warming.entrypoint.clone(),
            client_label: warming
                .current
                .as_ref()
                .and_then(|o| o.client_label.clone()),
            reason: warming.reason,
            last_guard: warming.last_guard,
            hold_counted: warming.hold_counted,
            paused: warming.paused,
            probes: warming.probes,
            usage: persisted_usage(&warming.usage),
            unanswered: warming
                .unanswered
                .iter()
                .map(|probe| PersistedUnanswered {
                    id: probe.id.clone(),
                    charge: PersistedCharge::of(&probe.charge),
                    unit: probe.unit,
                    window: Some(probe.window.to_string()),
                    fast: probe.fast,
                    model: probe.model.clone(),
                    expires: probe.expires,
                    covered: probe.covered,
                })
                .collect(),
            kept: warming
                .kept
                .iter()
                .map(|kept| PersistedKept {
                    key_generation: kept.key_generation,
                    pool_id: kept.pool_id.clone(),
                    canonical_model: kept.canonical_model.clone(),
                    charge: PersistedCharge::of(&kept.charge),
                    fast: kept.fast,
                    entry_expires: kept.entry_expires,
                    covered: kept.covered,
                    unit: kept.unit,
                    window: Some(kept.window.to_string()),
                    pending: kept.pending,
                })
                .collect(),
            last_real_at: warming.last_real_at,
            last_answered_at: warming.last_answered_at,
            last_stop: warming.last_stop,
            last_presence_at: agent.last_presence_at,
            last_probe_at: warming.last_probe_at,
            probes_today: warming.probes_today,
            credited_today: warming.credited_today,
            counters_day: warming.counters_day,
        }
    }

    /// The agent the payload describes, without its observation, and the observation's own fields apart with its
    /// client label, since the recipe they ride on comes from the row's columns. `None` when a word in the payload is
    /// one this build does not know. The timing is the engine's, the binary's fixed numbers, which the build a restart
    /// brings may have changed.
    fn restore(self, timing: KeepaliveTiming) -> Option<Decoded> {
        let mut agent = Agent::new(&self.id, &self.key_id, self.incarnation, self.last_activity);
        let mut warming = AgentWarming::new(self.last_activity, timing);
        warming.key_generation = self.key_generation;
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
        warming.idle_episode = self.idle_episode.map(|episode| IdleEpisode {
            started: episode.started,
            bucket: episode.bucket,
            anchor: episode
                .anchor
                .map(|anchor| anchor.restore(episode.anchor_marked)),
            prompt_at: episode.prompt_at,
            pool_key: episode.pool_key,
            bucket_day: episode.bucket_day,
        });
        warming.idle_window_floor = self.idle_window_floor;
        warming.last_prefix = self
            .last_prefix
            .map(|anchor| anchor.restore(self.last_prefix_marked));
        warming.last_prompt_tokens = self.last_prompt_tokens;
        agent.generation = self.generation;
        warming.expires = self.expires;
        agent.leaves_at = self.leaves_at;
        warming.jitter_frac = self.jitter_frac;
        warming.retry_at = self.retry_at;
        warming.cache_started = self.cache_started;
        warming.ttl_ms = self.ttl_ms;
        warming.cold = self.cold;
        warming.failures = self.failures;
        warming.account = self.account;
        warming.account_incarnation = self.account_incarnation;
        warming.pool_id = self.pool_id;
        warming.canonical_model = self.canonical_model;
        warming.reason = self.reason;
        warming.last_guard = self.last_guard;
        warming.entrypoint = self.entrypoint;
        warming.hold_counted = self.hold_counted;
        warming.paused = self.paused;
        warming.probes = self.probes;
        warming.usage = probe_usage(self.usage);
        warming.unanswered = self
            .unanswered
            .into_iter()
            .map(|probe| {
                Some(UnansweredProbe {
                    id: probe.id,
                    charge: probe.charge.restore(),
                    unit: probe.unit,
                    window: window_of(probe.window)?,
                    fast: probe.fast,
                    model: probe.model,
                    expires: probe.expires,
                    covered: probe.covered,
                })
            })
            .collect::<Option<_>>()?;
        warming.kept = self
            .kept
            .into_iter()
            .map(|kept| {
                Some(Kept {
                    key_generation: kept.key_generation,
                    pool_id: kept.pool_id,
                    canonical_model: kept.canonical_model,
                    charge: kept.charge.restore(),
                    fast: kept.fast,
                    entry_expires: kept.entry_expires,
                    covered: kept.covered,
                    unit: kept.unit,
                    window: window_of(kept.window)?,
                    pending: kept.pending,
                })
            })
            .collect::<Option<_>>()?;
        warming.last_real_at = self.last_real_at;
        warming.last_answered_at = self.last_answered_at;
        warming.last_stop = self.last_stop;
        agent.last_presence_at = self.last_presence_at;
        warming.last_probe_at = self.last_probe_at;
        warming.probes_today = self.probes_today;
        warming.credited_today = self.credited_today;
        warming.counters_day = self.counters_day;
        let current = self.current.map(|current| (current, self.client_label));
        Some((agent, warming, current))
    }
}

impl PersistedObservation {
    /// The observation over the recipe the row's columns carry: the opened prompt, the header block, the size, the
    /// speed and the dispatch TTL.
    fn restore(
        self,
        row: &KeepaliveAgentRow,
        body: Vec<u8>,
        client_label: Option<String>,
    ) -> Option<Observation> {
        let headers: Vec<(String, String)> = serde_json::from_str(&row.headers).ok()?;
        let prior = match self.prior {
            None => None,
            Some(prior) => Some(PriorCoverage {
                account: prior.account,
                covered: prior.covered,
                unit: optional(prior.unit, unit_of)?,
                window: window_of(prior.window)?,
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
                body: Bytes::from(body),
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
            funding: AttemptFunding {
                kind: optional(self.kind, phoebis_store::AccountKind::parse)?,
                usage_based: self.usage_based,
                in_overage: self.in_overage,
            },
            client_label,
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
        payload: serde_json::to_string(&PersistedSession::of(key, session))?,
        updated_at: now,
        seen_at: Some(session.last_report_at),
    })
}

/// One agent row as the store keeps it: the payload, the recipe's columns when one is retained, and its prompt only
/// when the recipe changed since the last landed save that carried it. That prompt comes back as a handle beside the
/// row, whose `Seal` holds an empty placeholder, so the copy of the body runs after the engine's lock is released.
fn agent_row(
    key: &PersistKey,
    agent: &Agent,
    warming: &AgentWarming,
    now: i64,
) -> Result<(KeepaliveAgentRow, Option<Bytes>), serde_json::Error> {
    let recipe = warming.current.as_ref().map(|o| &o.recipe);
    let no_headers: &[(String, String)] = &[];
    let (body_write, sealed_body) = match recipe {
        Some(recipe) if warming.body_dirty => {
            (BodyWrite::Seal(Vec::new()), Some(recipe.body.clone()))
        }
        None if warming.body_saved => (BodyWrite::Clear, None),
        _ => (BodyWrite::Keep, None),
    };
    let row = KeepaliveAgentRow {
        key_id: key.0.clone(),
        key_generation: key.1,
        session_id: key.2.clone(),
        agent_id: agent.id.clone(),
        record_version: RECORD_VERSION,
        payload: serde_json::to_string(&PersistedAgent::of(agent, warming))?,
        body: None,
        body_write,
        headers: serde_json::to_string(recipe.map_or(no_headers, |r| r.headers.as_slice()))?,
        body_size: recipe.map_or(0, |r| r.size),
        dispatched_ms: warming.current.as_ref().and_then(|o| o.dispatched_ms),
        fast: recipe.is_some_and(|r| r.fast),
        updated_at: now,
    };
    Ok((row, sealed_body))
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
    /// The body each sealing row of `batch.agents` carries, by the row's index, copied into its placeholder once the
    /// engine's lock is released.
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

/// The rows the store answered at start, sessions, agents and counters.
type Loaded = (
    Vec<KeepaliveSessionRow>,
    Vec<KeepaliveAgentRow>,
    Vec<IdleReturnsRow>,
);

/// An agent row read back: the agent, and its observation's fields with its client label when it carried a recipe.
type Decoded = (
    Agent,
    AgentWarming,
    Option<(PersistedObservation, Option<String>)>,
);

/// The agent a row describes, `None` when its version or a word in its payload is one this build does not read.
fn decode_agent(row: &KeepaliveAgentRow, timing: KeepaliveTiming) -> Option<Decoded> {
    (row.record_version == RECORD_VERSION)
        .then(|| serde_json::from_str::<PersistedAgent>(&row.payload).ok())
        .flatten()
        .and_then(|payload| payload.restore(timing))
}

fn persist_key_of(key: &SessionKey, session: &Session) -> PersistKey {
    (key.0.clone(), session.key_generation, key.1.clone())
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
            .entries
            .get(key)
            .map(|session| persist_key_of(key, session))
    }

    fn session_of(&self, key: &PersistKey) -> Option<&Session> {
        self.sessions
            .entries
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
        self.dirty.facts.retain(|key, _| {
            snapshot.key(&key.key_id).map(|k| k.created_at) == Some(key.key_generation)
        });
    }

    /// The batch the dirty sets name, read as the map stands now, each row with the revision it was read at, and the
    /// settlements the load held for a save. No dirty entry is cleared here: `acknowledge` does that once the save has
    /// landed. A dirty row whose record left the map without a removal, which only `close` does, is carried by nothing
    /// and retired with the rest. A filed set of reported facts goes as its report filed it, whatever the map holds.
    pub(crate) fn persist_snapshot(&mut self, now: i64) -> Snapshot {
        let mut snapshot = Snapshot {
            settled: std::mem::take(&mut self.warming.settlements_after_save),
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
                let warming = warming_ref(
                    &self.warming.agents,
                    &(row.0.clone(), row.2.clone()),
                    agent_id,
                )
                .expect(AGENT_HELD);
                match agent_row(row, agent, warming, now) {
                    Ok((agent_row, body)) => {
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
                        if let Some(body) = body {
                            snapshot.seals.push((snapshot.batch.agents.len(), body));
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
            for (day, counters) in self.warming.idle_returns.get(pool_id).into_iter().flatten() {
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
        snapshot.purge_counters_before = self.warming.purge_counters_before;
        snapshot.batch.purge_counters_before = self.warming.purge_counters_before;
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
            if let Some(warming) = self
                .sessions
                .entries
                .get(&(row.0.clone(), row.2.clone()))
                .filter(|session| session.key_generation == row.1)
                .and_then(|session| session.agents.get(agent_id))
                .map(|_| {
                    warming_mut(
                        &mut self.warming.agents,
                        &(row.0.clone(), row.2.clone()),
                        agent_id,
                    )
                    .expect(AGENT_HELD)
                })
            {
                warming.body_saved = *sealed;
                if unchanged {
                    warming.body_dirty = false;
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
        // Cleared only while unchanged since the snapshot: a roll in between recorded a later cutoff, which the next
        // save still owes.
        if self.warming.purge_counters_before == snapshot.purge_counters_before {
            self.warming.purge_counters_before = None;
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
            *self.warming.reasons.entry(Reason::Evicted).or_default() += 1;
        }
        let Some((_, mut warming, _)) = decoded else {
            return;
        };
        self.warming
            .settlements
            .extend(warming.settle_all(key, Wasted::Evicted));
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
    /// released records whose leave has passed, rows of an older version and rows this build cannot read go too.
    fn restore_persisted(&mut self, (sessions, agents, counters): Loaded, now: i64) {
        let (limits, timing) = (
            KeepaliveLimits {
                max_agents: self.sessions.max_agents,
                max_retained_bytes: self.warming.max_retained_bytes,
                max_sessions: self.sessions.max_sessions,
            },
            self.warming.timing,
        );
        for row in counters {
            let counters = self
                .warming
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
            if payload.ended || self.sessions.entries.contains_key(&key) {
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
            let mut session = payload.restore(now);
            let mut warming_agents = BTreeMap::new();
            // A session that has sent no real request the caps admit holds no slot of either cap, so it passes both
            // bounds and counts toward neither.
            let holds_slot = session.holds_slot();
            let over = if !holds_slot {
                None
            } else if live_sessions >= limits.max_sessions {
                Some(&mut by_sessions)
            } else if agent_count >= limits.max_agents {
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
            for (mut agent_row, decoded) in agents {
                let Some((agent, mut warming, observation)) = decoded else {
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
                warming.body_saved = agent_row.body_size > 0;
                if holds_slot && agent_count >= limits.max_agents {
                    self.dispose_loaded(
                        &key,
                        &agent_row,
                        Some((agent, warming, observation)),
                        &mut by_agents,
                    );
                    self.file_removed_agent(stored.clone(), agent_row.agent_id);
                    continue;
                }
                self.sessions.next_incarnation =
                    self.sessions.next_incarnation.max(agent.incarnation);
                let agent_id = agent_row.agent_id.clone();
                // The observation's restore reads the row's other columns alone, so the opened prompt moves out.
                match (observation, agent_row.body.take()) {
                    (None, _) => {}
                    (Some((observation, client_label)), Some(body)) => {
                        match observation.restore(&agent_row, body, client_label) {
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
            if self.warming.retained > limits.max_retained_bytes {
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
            if !inner.sessions.entries.is_empty() {
                return;
            }
            inner.restore_persisted(loaded, now);
            // The registry first, then the ledger: the settlements wait for the save that carries each ended recipe's
            // agent without the probes they settle, so a failed save sends nothing and a crash between the save and
            // the ledger's write leaves a wasted probe unwritten rather than written again by the next load.
            let settled = std::mem::take(&mut inner.warming.settlements);
            inner.warming.settlements_after_save.extend(settled);
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
            snapshot.batch.agents[index].body_write = BodyWrite::Seal(body.to_vec());
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
                let queued = std::mem::replace(&mut inner.warming.settlements_after_save, settled);
                inner.warming.settlements_after_save.extend(queued);
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
        let agent = Agent::new("main", "k", 1, 0);
        let mut warming = AgentWarming::new(0, KeepaliveTiming::default());
        warming.entrypoint = Some("cli".into());
        let payload = serde_json::to_value(PersistedAgent::of(&agent, &warming)).unwrap();
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
        let (_, restored, _) = loaded
            .restore(KeepaliveTiming::default())
            .expect("every word of the payload is known");
        assert_eq!(
            restored.entrypoint, None,
            "its probes price at the base regime until the next request names one"
        );
    }

    #[test]
    fn a_settlement_payload_without_a_window_reads_five_hour_and_one_naming_the_week_keeps_it() {
        let agent = Agent::new("main", "k", 1, 0);
        let mut warming = AgentWarming::new(0, KeepaliveTiming::default());
        warming.unanswered.push(UnansweredProbe {
            id: "probe".into(),
            charge: ProbeCharge {
                account: Some("o".into()),
                at: 0,
                band: Band::Standard,
                client: RowClient {
                    client: None,
                    user_agent: None,
                },
                funding: AttemptFunding::default(),
                tier: Some("default_claude_max_20x".into()),
                usage: ProbeUsage::default(),
            },
            unit: Some(SavingsUnit::WindowShare),
            window: SEVEN_DAY,
            fast: false,
            model: "claude-opus-5-5".into(),
            expires: 0,
            covered: true,
        });
        let payload = serde_json::to_value(PersistedAgent::of(&agent, &warming)).unwrap();
        assert_eq!(
            payload["unanswered"][0]["window"].as_str(),
            Some("seven_day"),
            "the settlement names its window beside its unit: {}",
            payload["unanswered"]
        );
        let window_of_restored = |payload: serde_json::Value| {
            serde_json::from_value::<PersistedAgent>(payload)
                .expect("the payload loads")
                .restore(KeepaliveTiming::default())
                .map(|(_, warming, _)| warming.unanswered[0].window)
        };
        assert_eq!(
            window_of_restored(payload.clone()),
            Some(SEVEN_DAY),
            "a payload naming the week keeps it"
        );
        let mut before = payload.clone();
        before["unanswered"][0]
            .as_object_mut()
            .expect("the entry is an object")
            .remove("window");
        assert_eq!(
            window_of_restored(before),
            Some(FIVE_HOUR),
            "an entry the earlier build saved was priced in the 5-hour window"
        );
        let mut unknown = payload;
        unknown["unanswered"][0]["window"] = "fortnight".into();
        assert_eq!(
            window_of_restored(unknown),
            None,
            "a window this build does not know refuses the row"
        );
    }
}
