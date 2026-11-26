//! The cache warming's saved rows: its row of each agent, with the retained prompt and its columns beside the payload,
//! and the idle-age counters, with what the load disposes of and what an acknowledged save settles.

use bytes::Bytes;
use phoebis_ledger::{Band, Speed};
use phoebis_routing::{FIVE_HOUR, SEVEN_DAY};
use phoebis_store::engine_state::BodyWrite;
use phoebis_store::{IdleReturnsRow, KeepaliveAgentRow};
use serde::{Deserialize, Serialize};

use crate::sessions::persist::{versioned, PersistKey};
use crate::sessions::state::{Agent, SessionKey, AGENT_HELD};
use crate::warming::agent::{warming_mut, AgentWarming};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::KeepaliveTiming;
use crate::warming::guard::Guard;
use crate::warming::idle::{IdleEpisode, IdleReturnBucket, PrefixAnchor};
use crate::warming::observation::{
    Kept, Observation, PriorCoverage, ProbeCharge, Settlement, UnansweredProbe,
};
use crate::warming::recipe::Recipe;
use crate::warming::savings::{AttemptFunding, RowClient, SavingsUnit};
use crate::warming::state::WarmingState;
use crate::warming::usage::ProbeUsage;

/// The shape of the warming's agent payload. A row of another version is dropped at the load, and the agent's row of
/// the session engine with it: version 1 carried no funding and version 2 a dollar in place of the probe's usage, tier,
/// instant and band, so neither settles under this build's rule.
pub(crate) const WARMING_RECORD_VERSION: u32 = 3;

/// The warming's saved members of an agent, from its `AgentWarming`, with the observation's fields and the recipe's
/// own beside them. The prompt, its header block, its size, its speed and the dispatch TTL are the row's columns, so
/// the body is sealed apart from the payload.
#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedWarming {
    pub key_generation: i64,
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
    pub current: Option<PersistedObservation>,
    pub expires: Option<i64>,
    #[serde(with = "exact")]
    pub jitter_frac: f64,
    pub retry_at: Option<i64>,
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
    /// `anchor`'s marked boundary, absent and loaded as `None` as `PersistedWarming.last_prefix_marked` is, so the
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
    /// Saved at the payload's top level as `PersistedWarming::client_label`, which `PersistedWarming::restore` copies
    /// in here.
    #[serde(skip)]
    pub client_label: Option<String>,
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

/// A warming row read back: the record, and its observation's fields with its client label when it carried a recipe.
pub(crate) type Restored = (AgentWarming, Option<PersistedObservation>);

impl PersistedWarming {
    fn of(warming: &AgentWarming) -> PersistedWarming {
        PersistedWarming {
            key_generation: warming.key_generation,
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
                client_label: None,
            }),
            expires: warming.expires,
            jitter_frac: warming.jitter_frac,
            retry_at: warming.retry_at,
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
            last_probe_at: warming.last_probe_at,
            probes_today: warming.probes_today,
            credited_today: warming.credited_today,
            counters_day: warming.counters_day,
        }
    }

    /// The warming's record the payload describes, without its observation, and the observation's own fields apart
    /// with its client label, since the recipe they ride on comes from the row's columns. `None` when a word in the
    /// payload is one this build does not know. The timing is the engine's, the binary's fixed numbers, which the build
    /// a restart brings may have changed.
    fn restore(self, timing: KeepaliveTiming) -> Option<Restored> {
        let mut warming = AgentWarming::new(self.cache_started, timing);
        warming.key_generation = self.key_generation;
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
        warming.expires = self.expires;
        warming.jitter_frac = self.jitter_frac;
        warming.retry_at = self.retry_at;
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
        warming.last_probe_at = self.last_probe_at;
        warming.probes_today = self.probes_today;
        warming.credited_today = self.credited_today;
        warming.counters_day = self.counters_day;
        let current = self.current.map(|current| PersistedObservation {
            client_label: self.client_label,
            ..current
        });
        Some((warming, current))
    }
}

impl PersistedObservation {
    /// The observation over the recipe the row's columns carry: the opened prompt, the header block, the size, the
    /// speed and the dispatch TTL.
    pub(crate) fn restore(self, row: &KeepaliveAgentRow, body: Vec<u8>) -> Option<Observation> {
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
            client_label: self.client_label,
        })
    }
}

/// One agent row of the warming as the store keeps it: the payload, the recipe's columns when one is retained, and its
/// prompt only when the recipe changed since the last landed save that carried it. That prompt comes back as a handle
/// beside the row, whose `Seal` holds an empty placeholder, so the copy of the body runs after the engine's lock is
/// released.
pub(crate) fn agent_row(
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
        record_version: WARMING_RECORD_VERSION,
        payload: serde_json::to_string(&PersistedWarming::of(warming))?,
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

/// The warming's record a row describes, `None` when its version or a word in its payload is one this build does not
/// read.
pub(crate) fn decode_warming(row: &KeepaliveAgentRow, timing: KeepaliveTiming) -> Option<Restored> {
    versioned::<PersistedWarming>(row.record_version, WARMING_RECORD_VERSION, &row.payload)
        .and_then(|payload| payload.restore(timing))
}

/// What the load left out to stay within the limits the file sets now, for the one line each limit logs.
#[derive(Default)]
pub(crate) struct Disposed {
    pub sessions: usize,
    pub agents: usize,
    pub recipes: usize,
}

impl WarmingState {
    /// The rows of the pool's idle-age counters, one per day and bucket.
    pub(crate) fn counter_rows(&self, pool_id: &str) -> Vec<IdleReturnsRow> {
        let mut rows = Vec::new();
        for (day, counters) in self.idle_returns.get(pool_id).into_iter().flatten() {
            for (bucket, counter) in counters.buckets.iter().enumerate() {
                rows.push(IdleReturnsRow {
                    pool_id: pool_id.to_string(),
                    day: *day,
                    bucket: bucket as u32,
                    reached: counter.reached,
                    returned: counter.returned,
                });
            }
        }
        rows
    }

    /// The settlements the load held for a save, which a snapshot takes under the lock it is read under.
    pub(crate) fn take_settlements_after_save(&mut self) -> Vec<Settlement> {
        std::mem::take(&mut self.settlements_after_save)
    }

    /// Holds each settlement queued for the ledger until a save carries the agents without the probes they settle.
    pub(crate) fn hold_settlements_for_save(&mut self) {
        let settled = std::mem::take(&mut self.settlements);
        self.settlements_after_save.extend(settled);
    }

    /// Puts the settlements a failed save took back in front of the ones held since its snapshot.
    pub(crate) fn return_unsaved_settlements(&mut self, settled: Vec<Settlement>) {
        let queued = std::mem::replace(&mut self.settlements_after_save, settled);
        self.settlements_after_save.extend(queued);
    }

    /// The cutoff of the days the last roll dropped, which the next save purges.
    pub(crate) fn purge_cutoff(&self) -> Option<i64> {
        self.purge_counters_before
    }

    /// Settles the marks of an agent's sealed body behind a landed save: the body it sealed or cleared is the
    /// registry's whatever moved since, while the mark asking for a seal leaves only where the agent's row is
    /// `unchanged` since the snapshot, since a recipe set after it is a body this save did not carry.
    pub(crate) fn settle_body(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        sealed: bool,
        unchanged: bool,
    ) {
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        warming.body_saved = sealed;
        if unchanged {
            warming.body_dirty = false;
        }
    }

    /// Clears the purge's cutoff behind the landed save that carried `saved`.
    pub(crate) fn purge_landed(&mut self, saved: Option<i64>) {
        // Cleared only while unchanged since the snapshot: a roll in between recorded a later cutoff, which the next
        // save still owes.
        if self.purge_counters_before == saved {
            self.purge_counters_before = None;
        }
    }

    /// The counters rows the store answered, into the idle-age counters.
    pub(crate) fn restore_counters(&mut self, counters: Vec<IdleReturnsRow>) {
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
    }

    /// An agent row a limit leaves out of the load. Its recipe counts as evicted, and the probes it left unanswered and
    /// the ones a request kept settle under `evicted`, as they do when the retained bytes evict a recipe through
    /// `discard`. The row itself goes with its session's removal or the caller's.
    pub(crate) fn dispose_loaded(
        &mut self,
        key: &SessionKey,
        row: &KeepaliveAgentRow,
        warming: Option<AgentWarming>,
        tally: &mut Disposed,
    ) {
        tally.agents += 1;
        if row.body_size > 0 {
            tally.recipes += 1;
            #[cfg(any(test, feature = "debug-hooks"))]
            self.count(Reason::Evicted);
        }
        let Some(mut warming) = warming else {
            return;
        };
        self.settlements
            .extend(warming.settle_all(key, Wasted::Evicted));
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
        let mut warming = AgentWarming::new(0, KeepaliveTiming::default());
        warming.entrypoint = Some("cli".into());
        let payload = serde_json::to_value(PersistedWarming::of(&warming)).unwrap();
        let saved: PersistedWarming = serde_json::from_value(payload.clone()).unwrap();
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
        let loaded: PersistedWarming =
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

    #[test]
    fn a_settlement_payload_without_a_window_reads_five_hour_and_one_naming_the_week_keeps_it() {
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
        let payload = serde_json::to_value(PersistedWarming::of(&warming)).unwrap();
        assert_eq!(
            payload["unanswered"][0]["window"].as_str(),
            Some("seven_day"),
            "the settlement names its window beside its unit: {}",
            payload["unanswered"]
        );
        let window_of_restored = |payload: serde_json::Value| {
            serde_json::from_value::<PersistedWarming>(payload)
                .expect("the payload loads")
                .restore(KeepaliveTiming::default())
                .map(|(warming, _)| warming.unanswered[0].window)
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
