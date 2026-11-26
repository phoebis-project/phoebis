//! A real request the warming follows from its arrival to its completion, the recipe it leaves, the probes that
//! recipe paid for and how a stop settles them.

use std::collections::{BTreeSet, HashMap};

use phoebis_bus::user_agent_of;
use phoebis_ledger::Band;
use phoebis_routing::SEVEN_DAY;

use crate::sessions::state::{agent_ref, AgentState, SessionKey, SessionsState, AGENT_HELD};
use crate::warming::agent::{warming_mut, warming_ref, AgentWarming};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::floor_ttl_ms;
use crate::warming::recipe::Recipe;
use crate::warming::savings::{AttemptFunding, RowClient, SavingsUnit};
use crate::warming::state::{Changes, WarmingState};
use crate::warming::usage::ProbeUsage;
use crate::warming::view::{recipe_state, RecipeState};

/// The share of the previous main prompt a completion must read from the cache, under a chain that holds none of the
/// anchor's boundaries, for the engine to read a contradiction between its chain model and the upstream's.
pub const CHAIN_CONTRADICTION_COVERAGE: f64 = 0.9;

/// What the agent's previous recipe had proved when this request arrived, taken at admission and consumed with
/// the observation. The kept-warm credit reads nothing else: a probe that runs while this request is open, or a
/// TTL this request's own evidence shortens, cannot reach into it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PriorCoverage {
    pub account: Option<String>,
    pub covered: bool,
    /// The unit the covering probe's cost settled in, decided by the kind the probe's answer carried, so an account
    /// that leaves the registry while its probe is in flight still lands a unit. `None` when the answer named no
    /// account (one the relay built before any selection), in which case the credit never fires.
    pub unit: Option<SavingsUnit>,
    /// The window a `WindowShare` settlement was priced in, `seven_day` for every entry this build writes and
    /// `five_hour` for one loaded from an agent the earlier build saved.
    pub window: &'static str,
    pub model: String,
    /// The covering recipe's speed, which the credited request's has to match.
    pub fast: bool,
    pub real_at: i64,
    pub ttl_ms: i64,
    pub until: Option<i64>,
}

impl PriorCoverage {
    /// The kept-warm credit's test for a request that started at `request_started` on `model` at the speed `fast`, was
    /// served by `account` and read `read` from the cache: it arrived after the previous turn's own entry would have
    /// lapsed and read the entry the covering probe kept.
    pub(crate) fn credits(
        &self,
        request_started: i64,
        model: &str,
        fast: bool,
        account: Option<&str>,
        read: u64,
    ) -> bool {
        let ttl = floor_ttl_ms(self.ttl_ms);
        let gap_exceeded = request_started - self.real_at > ttl;
        // The entry the probe kept has to be there when this request arrives, on the boundary the settlement and the
        // scheduler read (`answer.started` before `chain_end`, `now >= expires`): a request at or after the lapse read
        // a cache nothing the probe paid for kept, so that probe settles as expired, and a credit here would contradict
        // that settlement.
        let entry_alive = self.until.is_some_and(|until| request_started < until);
        let same_model = self.model == model;
        // The upstream shares no cached prefix across speeds, so a probe on the other lane kept nothing this request
        // read.
        let same_speed = self.fast == fast;
        // The account that served this request, after any failover, has to be the one the covering probe warmed. A
        // prefix cached elsewhere by ordinary traffic is no probe's to credit.
        let same_account = self.account.as_deref() == account;
        // A probe whose answer named no account (one the relay built before any selection) settled in no unit and
        // funds nothing to credit, whatever `same_account` says of the account it was meant for.
        self.covered
            && gap_exceeded
            && entry_alive
            && same_model
            && same_speed
            && same_account
            && read > 0
            && self.unit.is_some()
    }
}

/// The compaction the agent has open, held apart from the retained observation: its own receipt, start, lineage, head,
/// read and TTL, and the coverage its credit reads. The recipe and its probes wait as they stood until its completion
/// decides them, so the completion judges the agent's own lists.
pub(crate) struct OpenCompaction {
    pub request_id: String,
    /// The relay's receipt, the instant the compaction's own request row keys its band at.
    pub received_at: i64,
    /// When it reached the engine, the arrival its answer to the probes before it is judged by.
    pub started: i64,
    pub canonical_model: String,
    pub fast: bool,
    pub attempt: u8,
    pub account: Option<String>,
    pub funding: AttemptFunding,
    /// Whether the current attempt's head was a 2xx, without which its stream's usage is no evidence about the cache.
    pub ok_head: bool,
    /// What the current attempt's stream read from the cache, for a completion that carries no usage of its own.
    pub read: u64,
    /// The TTL the relay dispatched the attempt's body with, or where it went as sent, the one the recipe's evidence
    /// settled on, the same conversation's markers, refined by the compaction's own usage: the bucket its kept-warm
    /// credit files under.
    pub ttl_ms: i64,
    /// What the previous recipe had proved at the admission, as a turn's observation takes it.
    pub prior: Option<PriorCoverage>,
    /// The compaction request's own client label and user agent, the ones its kept-warm credit keys.
    pub client: RowClient,
}

/// What a request that completed answers the probes before it with: when it reached the cache, the lineage it read on
/// and what it read.
pub(crate) struct Answer {
    pub started: Option<i64>,
    pub account: Option<String>,
    pub model: String,
    pub fast: bool,
    pub read: u64,
}

/// A kept-warm credit a completion earned, its keys read under the lock and recorded once the lock is released. `at` is
/// the credited request's receipt, the instant its row files under, so the credit sits in that row's hour and revision
/// whenever the answer completes, and `ttl_ms` the TTL its own write would have taken. `funding` is its serving
/// attempt's, so the credit files under the funding word its request row took, and `client` its client label and user
/// agent as the relay observed them, so the credit keys its request row's client and agent and prices under that
/// client's regime.
pub(crate) struct Credit {
    pub(crate) at: i64,
    pub(crate) key: SessionKey,
    pub(crate) key_generation: i64,
    pub(crate) pool_id: String,
    pub(crate) account: Option<String>,
    pub(crate) model: String,
    pub(crate) read: u64,
    pub(crate) ttl_ms: i64,
    pub(crate) fast: bool,
    pub(crate) funding: AttemptFunding,
    pub(crate) client: RowClient,
}

/// Drops the agent's observation, with the pending entry it follows and the bytes its recipe held.
fn drop_current(
    warming: &mut AgentWarming,
    pending: &mut HashMap<String, (SessionKey, String)>,
    retained: &mut usize,
) {
    if let Some(observation) = warming.current.take() {
        pending.remove(&observation.id);
        *retained -= observation.recipe.size;
    }
}

/// Whether a request the engine follows is open on the agent of `warming`: the turn its observation follows, while
/// `pending` holds it, or a compaction. Its completion decides the probes and the recipe, so no lapse, eviction or
/// sweep decides them first.
fn request_open(pending: &HashMap<String, (SessionKey, String)>, warming: &AgentWarming) -> bool {
    warming.compaction.is_some()
        || warming
            .current
            .as_ref()
            .is_some_and(|o| pending.contains_key(&o.id))
}

/// What the agent's previous recipe proved for a request admitted at `now`: the kept-warm credit reads this and nothing
/// on the agent. The newest unanswered probe whose coverage held is taken, a rewrite included, then the newest
/// unanswered probe where none held, so a late probe's `covered: false` never shadows an older covered one whose entry
/// still lives, and the coverage leaves with the probes the request keeps. A recipe the idle limit ended leaves its
/// probes kept and pending, as a failed turn hands back the probes it took, so the newest of those whose entry still
/// lives stands in for it. The gap runs from the last turn that answered, since a failed turn is no evidence about the
/// cache.
pub(crate) fn prior_coverage(warming: &AgentWarming, now: i64) -> Option<PriorCoverage> {
    let newest_kept = warming
        .kept
        .iter()
        .rev()
        .find(|kept| kept.pending.is_some() && now < kept.entry_expires);
    let newest_unanswered = warming
        .unanswered
        .iter()
        .rev()
        .find(|p| p.covered)
        .or(warming.unanswered.last());
    let answered_at = warming.last_answered_at.or(warming.last_real_at);
    answered_at.map(|real_at| {
        let ttl_ms = warming.ttl_ms;
        match (newest_unanswered, newest_kept) {
            (Some(probe), _) => PriorCoverage {
                account: probe.charge.account.clone(),
                covered: probe.covered,
                unit: probe.unit,
                window: probe.window,
                model: probe.model.clone(),
                fast: probe.fast,
                real_at,
                ttl_ms,
                until: Some(probe.expires),
            },
            (None, Some(kept)) => PriorCoverage {
                account: kept.charge.account.clone(),
                covered: kept.covered,
                unit: kept.unit,
                window: kept.window,
                model: kept.canonical_model.clone(),
                fast: kept.fast,
                real_at,
                ttl_ms,
                until: Some(kept.entry_expires),
            },
            (None, None) => PriorCoverage {
                account: None,
                covered: false,
                unit: None,
                window: SEVEN_DAY,
                model: warming.canonical_model.clone(),
                fast: false,
                real_at,
                ttl_ms,
                until: None,
            },
        }
    })
}

/// One real request the engine is following from its observation to its completion.
pub(crate) struct Observation {
    pub id: String,
    pub attempt: u8,
    pub started: i64,
    /// The relay's receipt of the request, the instant its own request row keys its band at.
    pub received_at: i64,
    pub recipe: Recipe,
    pub ready: bool,
    pub expected: u64,
    pub ttl_ms: i64,
    pub prior: Option<PriorCoverage>,
    /// Observed inside a gap's backlog, so this request's own terminal event may be one the gap dropped:
    /// nothing it carries may arm a recipe.
    pub poisoned: bool,
    /// The TTL the relay actually dispatched this attempt's body with, read off `AccountSelected`, or `None` when
    /// the relay left the markers as sent, in which case the cap falls back to the recipe's own received TTL.
    pub dispatched_ms: Option<i64>,
    /// The kind and the class of the account the current attempt was placed on, read off `AccountSelected`, and
    /// whether that attempt's head reported overage in use, the fact the request's own row files under, so a skip and a
    /// kept-warm credit file under the funding this request ran under whatever the registry holds by then.
    pub funding: AttemptFunding,
    /// The request's client label, whole. Its kept-warm credit keys it, a probe takes it at dispatch for its
    /// settlement, and a skip reads it, since a probe replays this request's headers.
    pub client_label: Option<String>,
}

/// A main request whose chain held none of the anchor's boundaries at its arrival, kept until its completion.
pub(crate) struct ChainMark {
    pub session: SessionKey,
    pub previous_prompt: u64,
    pub arrived_at: i64,
}

/// The recipe a real request's arrival put aside, with the schedule and the error count it stood on and the model and
/// account it was armed under, restored where the request fails or is canceled and dropped where it succeeds, is
/// rejected, or the entry expired meanwhile. Its body stays counted in `WarmingState::retained` while it lives.
pub(crate) struct AsideRecipe {
    pub observation: Observation,
    pub expires: Option<i64>,
    pub retry_at: Option<i64>,
    pub jitter_frac: f64,
    pub ttl_ms: i64,
    pub cache_started: i64,
    pub failures: u32,
    pub canonical_model: String,
    pub entrypoint: Option<String>,
    pub account: Option<String>,
    /// The holder's incarnation beside `account`, so a recipe put back stands on the holder it was armed on.
    pub account_incarnation: Option<u64>,
    pub pool_id: String,
    pub key_generation: i64,
}

/// What one probe was charged under, off its `ProbeRecord`, carried whole by every copy of the probe so its
/// settlement files where its own cost row did.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProbeCharge {
    pub account: Option<String>,
    /// The probe's exchange instant and band, so a settlement filed a day later keys the probe's hour and prices its
    /// counts at the cells and the revision the probe ran at.
    pub at: i64,
    pub band: Band,
    /// The client and the user agent the probe's own request row keys, off its `ProbeRecord`, so a settlement filed
    /// after its recipe ended or past a restart keys them too.
    pub client: RowClient,
    /// The kind and the class the probe was served under and whether its answer reported overage, as the probe's own
    /// cost row took them, so the waste files under the same funding word whatever the registry holds now.
    pub funding: AttemptFunding,
    /// The account tier the settlement read, so the credit row keys the tier the probe ran under.
    pub tier: Option<String>,
    /// The probe's own usage, which the rollup carries as the wasted counts and prices when read.
    pub usage: ProbeUsage,
}

/// One unanswered probe with the label that settles it as wasted. Queued under the lock and written to the ledger by
/// the engine once the lock is released.
pub(crate) struct Settlement {
    pub key: SessionKey,
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub label: Wasted,
    pub charge: ProbeCharge,
}

/// A due probe a gate or a lapsed entry withheld, with the keys its skip row files under, read while the recipe still
/// held its speed.
pub(crate) struct Skipped {
    pub key: SessionKey,
    pub key_generation: i64,
    pub pool_id: String,
    pub account: Option<String>,
    pub canonical_model: String,
    pub fast: bool,
    pub state: RecipeState,
    /// The kind, the class and the overage of the request the recipe came from, off its observation, so the skip files
    /// under the funding that request ran under.
    pub funding: AttemptFunding,
    /// The observation's client label and `user_agent_of` over the recipe's headers, the client and the user agent the
    /// withheld probe's own request row would have keyed.
    pub client: RowClient,
}

impl Skipped {
    fn of(key: &SessionKey, warming: &AgentWarming, state: RecipeState) -> Skipped {
        let observation = warming.current.as_ref();
        Skipped {
            key: key.clone(),
            key_generation: warming.key_generation,
            pool_id: warming.pool_id.clone(),
            account: warming.account.clone(),
            canonical_model: warming.canonical_model.clone(),
            fast: observation.is_some_and(|o| o.recipe.fast),
            state,
            funding: observation.map(|o| o.funding).unwrap_or_default(),
            client: RowClient {
                client: observation.and_then(|o| o.client_label.clone()),
                user_agent: observation.and_then(|o| user_agent_of(&o.recipe.headers)),
            },
        }
    }
}

/// One probe the next real request has not answered yet: what it used and the unit it settled in, the lineage a
/// request has to match to be read as its answer, and the end of the entry it wrote, which a later probe of its chain
/// extends by reading the entry back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UnansweredProbe {
    /// The relay's request id of the probe's exchange, which the probe's own ledger request row carries, so a
    /// settlement of this probe can be told from a repeat of it.
    pub id: String,
    pub charge: ProbeCharge,
    pub unit: Option<SavingsUnit>,
    /// The window `unit` was priced in, as `PriorCoverage::window` reads it.
    pub window: &'static str,
    pub fast: bool,
    pub model: String,
    /// `job.started + ttl_ms` as the landing computes the entry's end, for a probe that read the whole prefix and
    /// for one that read nothing and rewrote it alike.
    pub expires: i64,
    /// Whether the probe established valid full-prefix coverage: false at the push, true once the landing's checks
    /// passed (a full read, or an accepted complete rewrite). A probe paused as `PartialCacheCoverage`,
    /// `NoProbeCacheEvidence`, an unexpected output or a missing account keeps false and is still paid for and
    /// settled, so the cost record and the coverage record never merge.
    pub covered: bool,
}

/// One unanswered probe with the rollup keys it was paid under, which its settlement files under.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Kept {
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub charge: ProbeCharge,
    pub fast: bool,
    /// When the entry this probe wrote expires. A later probe of its chain that read the entry back extended it, so a
    /// settlement reads the chain's last entry (`chain_expires`).
    pub entry_expires: i64,
    /// What the probe proved, carried from its `UnansweredProbe` by `Kept::of`, so a turn that arrives after the
    /// ceiling moved the probe here reads the evidence the unanswered list held.
    pub covered: bool,
    pub unit: Option<SavingsUnit>,
    /// The window `unit` was priced in, as `PriorCoverage::window` reads it.
    pub window: &'static str,
    /// The label a probe written off at a ceiling settles under when its chain's last entry lapses with no turn:
    /// `idle_limit` today. A turn that reads the probe leaves the label in place, so one that ends without deciding the
    /// probe hands it back to the lapse sweep. `None` for a probe a real request superseded, which waits for that
    /// request's completion. A failed completion hands such a probe back under `expired`, the label its recipe's own
    /// lapse settles it under. A lapse of the recipe hands a probe whose chain is still alive back the same way, under
    /// `expired`, for the sweep to settle at the chain's end.
    pub pending: Option<Wasted>,
}

impl Kept {
    pub(crate) fn of(warming: &AgentWarming, probe: UnansweredProbe) -> Kept {
        Kept {
            key_generation: warming.key_generation,
            pool_id: warming.pool_id.clone(),
            canonical_model: probe.model,
            charge: probe.charge,
            fast: probe.fast,
            entry_expires: probe.expires,
            covered: probe.covered,
            unit: probe.unit,
            window: probe.window,
            pending: None,
        }
    }

    pub(crate) fn settle(self, key: &SessionKey, label: Wasted) -> Settlement {
        Settlement {
            key: key.clone(),
            key_generation: self.key_generation,
            pool_id: self.pool_id,
            canonical_model: self.canonical_model,
            label,
            charge: self.charge,
        }
    }

    fn link(&self) -> ChainLink<'_> {
        ChainLink {
            at: self.charge.at,
            model: &self.canonical_model,
            fast: self.fast,
            account: self.charge.account.as_deref(),
            read: self.charge.usage.read,
            covered: self.covered,
            expires: self.entry_expires,
        }
    }
}

impl UnansweredProbe {
    /// The link the probe makes as a kept one, from the fields `Kept::of` carries over.
    fn link(&self) -> ChainLink<'_> {
        ChainLink {
            at: self.charge.at,
            model: &self.model,
            fast: self.fast,
            account: self.charge.account.as_deref(),
            read: self.charge.usage.read,
            covered: self.covered,
            expires: self.expires,
        }
    }
}

/// What `chain_expires` reads of one probe, borrowed off a kept or an unanswered probe so the per-tick lapse sweep
/// clones no probe to read its chain.
#[derive(Clone, Copy)]
struct ChainLink<'a> {
    at: i64,
    model: &'a str,
    fast: bool,
    account: Option<&'a str>,
    read: u64,
    covered: bool,
    expires: i64,
}

/// What ending an observation does with the probes the recipe leaves unanswered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// A labeled stop settles them and the kept ones under its label. A stop with no label drops them and leaves
    /// the kept ones for whatever decides them.
    Settle,
    /// A real request supersedes the recipe and keeps them until its completion decides them, a failed one handing
    /// them back pending for the next turn, or the idle limit or a lapse keeps them pending under its label until
    /// their entry lapses or a turn reads them.
    Keep,
    /// A bus gap or a failed request ends the observation without deciding the probes sent while it was open: they
    /// wait where they are, with the recipe's speed, for the next request.
    Leave,
}

impl WarmingState {
    /// The one writer of an agent's reason, so the counter behind the views never disagrees with the reason
    /// they show. Nearly every change to a recipe writes a reason, so this is where most of them become dirty.
    pub(crate) fn set_reason(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
    ) -> Changes {
        let mut changes = Changes::default();
        if let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        {
            warming.reason = reason;
        }
        *self.reasons.entry(reason).or_default() += 1;
        changes.agent_row(key, agent_id);
        changes
    }

    /// Counts one cause that no agent's reason takes.
    pub(crate) fn count(&mut self, reason: Reason) {
        *self.reasons.entry(reason).or_default() += 1;
    }

    /// Discards every agent of the session under `reason`, each one's probes settled under `wasted`.
    pub(crate) fn discard_session(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        reason: Reason,
        wasted: Option<Wasted>,
    ) -> Changes {
        let mut changes = Changes::default();
        let agents: Vec<String> = sessions
            .entries
            .get(key)
            .map(|session| session.agents.keys().cloned().collect())
            .unwrap_or_default();
        for agent in &agents {
            changes.append(self.discard(sessions, key, agent, reason, wasted));
        }
        changes
    }

    /// Discards each agent of the session that holds warming work, under `reason` and with its probes settled under
    /// `wasted`. An agent with nothing to drop keeps its generation and its reason, so its lifecycle word stands.
    pub(crate) fn discard_warming_of(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        reason: Reason,
        wasted: Wasted,
    ) -> Changes {
        let mut changes = Changes::default();
        let agents: Vec<String> = self
            .agents
            .get(key)
            .map(|warming_agents| {
                warming_agents
                    .iter()
                    .filter(|(_, warming)| warming.holds_warming_work())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for agent in &agents {
            changes.append(self.discard(sessions, key, agent, reason, Some(wasted)));
        }
        changes
    }

    /// Drops the agent's recipe and everything following it, and answers the observation's end for the caller to move
    /// the agent to a new generation, so no late result about the old one lands. `wasted` labels the probes the recipe
    /// leaves unanswered.
    pub(crate) fn discard(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        wasted: Option<Wasted>,
    ) -> Changes {
        let mut changes = self.end_observation(sessions, key, agent_id, wasted, Unanswered::Settle);
        changes.append(self.set_reason(sessions, key, agent_id, reason));
        changes
    }

    /// `discard` for a real request that supersedes the recipe: the probes the recipe leaves unanswered were paying
    /// for that request, so it keeps them until the first of these decides them: its successful completion (each
    /// probe by its chain's last entry: a warm return on its lineage inside that entry clears it, one past it settles
    /// it under `expired`, and any other return, on another lineage or with no read, under `cold_return`), a labeled
    /// discard (under its label), or the agent's lapse or eviction. An eviction settles them at once, and a lapse
    /// settles at once only those whose chain has ended and hands the rest back under `expired`. A request the engine
    /// takes no observation of leaves them kept for whichever comes next, and one that fails hands them back for the
    /// next turn to read or the lapse sweep to settle.
    pub(crate) fn discard_keeping_unanswered(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
    ) -> Changes {
        let mut changes = self.end_observation(sessions, key, agent_id, None, Unanswered::Keep);
        changes.append(self.set_reason(sessions, key, agent_id, reason));
        changes
    }

    /// `discard_keeping_unanswered` for an arrival, an armed recipe (unpaused, its entry ahead of `now`) set aside
    /// instead of ended: its observation leaves `current` and `pending` with its size kept in `retained`, and waits
    /// with its schedule and context for the completion to restore or drop it. A paused recipe and one that never armed
    /// end through the discard. An older recipe set aside over a completion the engine missed leaves through the
    /// discard.
    pub(crate) fn set_aside(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
    ) -> Changes {
        let aside = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
            .filter(|warming| {
                !warming.paused && warming.expires.is_some_and(|expires| expires > now)
            })
            .and_then(|warming| {
                let observation = warming.current.take()?;
                Some(AsideRecipe {
                    observation,
                    expires: warming.expires,
                    retry_at: warming.retry_at,
                    jitter_frac: warming.jitter_frac,
                    ttl_ms: warming.ttl_ms,
                    cache_started: warming.cache_started,
                    failures: warming.failures,
                    canonical_model: warming.canonical_model.clone(),
                    entrypoint: warming.entrypoint.clone(),
                    account: warming.account.clone(),
                    account_incarnation: warming.account_incarnation,
                    pool_id: warming.pool_id.clone(),
                    key_generation: warming.key_generation,
                })
            });
        if let Some(aside) = &aside {
            self.pending.remove(&aside.observation.id);
        }
        let changes =
            self.discard_keeping_unanswered(sessions, key, agent_id, Reason::AwaitingSelection);
        if let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        {
            warming.aside = aside;
        }
        changes
    }

    /// The recipe the arrival set aside, taken out for the completion to restore, its body still counted in
    /// `retained`.
    pub(crate) fn take_aside(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
    ) -> Option<AsideRecipe> {
        agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))?
            .aside
            .take()
    }

    /// Puts the recipe `take_aside` took back into `current` with the schedule, the error count and the model and
    /// account it stood on, where its entry is still ahead of `now`, and otherwise drops it, its size leaving
    /// `retained`. Its id never goes back into `pending`: its own request ended before the one that set it aside
    /// arrived, so the lapse, the eviction and the sweeps take it as any armed recipe. Answers whether it re-armed.
    pub(crate) fn rearm_aside(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        aside: AsideRecipe,
        now: i64,
    ) -> (Changes, bool) {
        let mut changes = Changes::default();
        let warming = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
            .filter(|_| aside.expires.is_some_and(|expires| expires > now));
        let Some(warming) = warming else {
            self.retained -= aside.observation.recipe.size;
            return (changes, false);
        };
        warming.current = Some(aside.observation);
        warming.expires = aside.expires;
        warming.retry_at = aside.retry_at;
        warming.jitter_frac = aside.jitter_frac;
        warming.ttl_ms = aside.ttl_ms;
        warming.cache_started = aside.cache_started;
        warming.failures = aside.failures;
        warming.canonical_model = aside.canonical_model;
        warming.entrypoint = aside.entrypoint;
        warming.account = aside.account;
        warming.account_incarnation = aside.account_incarnation;
        warming.pool_id = aside.pool_id;
        warming.key_generation = aside.key_generation;
        // The next save seals the restored body in place of the failed request's.
        warming.body_dirty = true;
        changes.agent_row(key, agent_id);
        (changes, true)
    }

    /// Drops the recipe the arrival set aside, its size leaving `retained`.
    pub(crate) fn drop_aside(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
    ) {
        if let Some(aside) = self.take_aside(sessions, key, agent_id) {
            self.retained -= aside.observation.recipe.size;
        }
    }

    /// The entry a kept probe is judged against: the greatest `entry_expires` among the probes of its chain. The chain
    /// is the probe and each later probe of its lineage (model, speed, account) in `chain`, in exchange order, up to
    /// the first whose read did not cover the entry: a probe that read the whole entry back extended it, while one that
    /// read nothing or only part of the prefix found the entry gone, whole or in part, and wrote a new one, which
    /// starts a chain of its own. The read is tested beside `covered`, since the landing marks a cold complete rewrite
    /// covered too. The chain also runs through the lineage's unanswered probes where the caller hands them in beside
    /// the kept ones, as a compaction's completion, a lapse and the sweep do, since the recipe a failed turn re-arms
    /// keeps probing the entry its kept probes paid for.
    fn chain_expires(chain: &[ChainLink], probe: ChainLink) -> i64 {
        let mut later: Vec<ChainLink> = chain
            .iter()
            .copied()
            .filter(|other| {
                other.at > probe.at
                    && other.model == probe.model
                    && other.fast == probe.fast
                    && other.account == probe.account
            })
            .collect();
        later.sort_by_key(|other| other.at);
        later
            .into_iter()
            .take_while(|other| other.read > 0 && other.covered)
            .map(|other| other.expires)
            .fold(probe.expires, i64::max)
    }

    /// The decision `discard_keeping_unanswered` defers, taken by the successful completion of the request that
    /// kept the probes, `read` being what the completion read from the cache. Each probe answers to the lineage it
    /// was paid under, since a request that failed leaves its probes behind with the agent already moved to that
    /// request's model and entry, and each probe of a chain is judged against the chain's last entry
    /// (`chain_expires`): a warm return is one on the probe's model, speed and account, arriving before that entry
    /// expired, that read the cache. A kept-warm credit takes no shortcut here: a probe off the lineage the credited
    /// request read kept nothing it read. The model is the probe's as resolutions followed it. A probe sent while the
    /// request was open is not among them: it pays for the request after this one.
    pub(crate) fn decide_unanswered(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        read: u64,
    ) -> Changes {
        let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        else {
            return Changes::default();
        };
        let answer = Answer {
            // The read happened when the request reached the cache, so a stream that outlives the entry still read it.
            started: warming.current.as_ref().map(|o| o.started),
            account: warming.account.clone(),
            model: warming.canonical_model.clone(),
            fast: warming.fast(),
            read,
        };
        let probes = std::mem::take(&mut warming.kept);
        self.decide_against(key, agent_id, probes, answer)
    }

    /// `decide_unanswered`'s test over `probes` against the arrival, the lineage and the read `answer` holds: a turn's
    /// observation and its agent's, or a compaction's own. `probes` is every probe the request answers in one slice,
    /// so a chain that runs from the kept probes into the unanswered ones settles against its last entry.
    pub(crate) fn decide_against(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        probes: Vec<Kept>,
        answer: Answer,
    ) -> Changes {
        let mut changes = Changes::default();
        changes.agent_row(key, agent_id);
        let links: Vec<ChainLink> = probes.iter().map(Kept::link).collect();
        let chain_ends: Vec<i64> = links
            .iter()
            .map(|&probe| Self::chain_expires(&links, probe))
            .collect();
        for (kept, chain_end) in probes.into_iter().zip(chain_ends) {
            let lineage = kept.canonical_model == answer.model
                && kept.fast == answer.fast
                && kept.charge.account == answer.account;
            // A probe whose chain's last entry was gone when the request arrived kept nothing the request could read,
            // whatever it read: `expired` is its own label, so a credited request never sits beside a cold return it
            // contradicts.
            let label = match answer.started {
                Some(at) if at >= chain_end => Some(Wasted::Expired),
                Some(_) if answer.read > 0 && lineage => None,
                _ => Some(Wasted::ColdReturn),
            };
            if let Some(label) = label {
                self.settlements.push(kept.settle(key, label));
            }
        }
        changes
    }

    /// `discard`'s bookkeeping without writing a reason: for a terminal state that is already the agent's
    /// current reason, so ending the observation does not count that reason a second time.
    pub(crate) fn end_observation(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        wasted: Option<Wasted>,
        unanswered: Unanswered,
    ) -> Changes {
        let mut changes = Changes::default();
        changes.agent_row(key, agent_id);
        // A labeled stop settles or writes off the probes an open compaction would answer, so the compaction ends as a
        // failed one, since its completion would credit a probe the stop wasted and relabel the stop. A registration
        // that takes a released agent back follows such a stop, so it finds no compaction open.
        if wasted.is_some() {
            self.end_compaction(sessions, key, agent_id);
        }
        // Whatever ends the observation leaves nothing for a recipe set aside to wait for, so its body leaves memory
        // with it.
        self.drop_aside(sessions, key, agent_id);
        if agent_ref(&sessions.entries, key, agent_id).is_none() {
            return changes;
        }
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        drop_current(warming, &mut self.pending, &mut self.retained);
        warming.expires = None;
        warming.retry_at = None;
        changes.ended(key, agent_id);
        warming.paused = false;
        warming.last_stop = wasted;
        match (unanswered, wasted) {
            (Unanswered::Leave, _) => {}
            (Unanswered::Keep, _) => {
                // A row apiece: a request whose completion never came leaves its probes for the next one, which may
                // keep probes paid under other keys, and each settles by its chain's last entry. A ceiling's label
                // rides along, for the lapse sweep to settle under when no turn reads them first.
                let probes = warming.take_unanswered();
                warming.kept.extend(probes.into_iter().map(|kept| Kept {
                    pending: wasted,
                    ..kept
                }));
            }
            (Unanswered::Settle, Some(label)) => {
                self.settlements.extend(warming.settle_all(key, label));
            }
            (Unanswered::Settle, None) => warming.unanswered.clear(),
        }
        changes
    }

    /// Ends the compaction open on the agent as a failed one: it leaves the agent and the map, settles nothing and
    /// leaves the recipe as it stood, so its later events miss both maps as an unknown request's do.
    pub(crate) fn end_compaction(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
    ) {
        let Some(open) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
            .and_then(|warming| warming.compaction.take())
        else {
            return;
        };
        self.compactions.remove(&open.request_id);
    }

    /// Settles every kept probe whose `pending` label is set and whose chain's last entry (`chain_expires`) ended at or
    /// before `now` under that label, on each agent with no request open, and touches each agent it settled for the
    /// next save, which releases the settlements. A probe whose own entry ended inside a live chain waits with the
    /// chain, so a turn inside the chain's last entry still answers every probe of it. The chain runs through the
    /// agent's unanswered probes of the lineage too, since the recipe a failed turn re-arms keeps probing, which is the
    /// chain `decide_against` judges once the next arrival moves those probes into `kept`. Only kept probes settle.
    pub(crate) fn settle_lapsed_pending(&mut self, sessions: &SessionsState, now: i64) -> Changes {
        let mut changes = Changes::default();
        let open = &self.pending;
        let mut settled: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in sessions.entries.iter() {
            for agent_id in session.agents.keys() {
                let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
                // An open turn's or compaction's completion decides the probes, as `lapse` waits for it. A request that
                // fails, past a head or not, or ends at a bus gap decides nothing and leaves them here.
                // The sweep settles labeled probes alone, so an agent holding none builds no chain.
                if request_open(open, warming)
                    || !warming.kept.iter().any(|kept| kept.pending.is_some())
                {
                    continue;
                }
                let chain: Vec<ChainLink> = warming
                    .kept
                    .iter()
                    .map(Kept::link)
                    .chain(warming.unanswered.iter().map(UnansweredProbe::link))
                    .collect();
                let chain_ends: Vec<i64> = warming
                    .kept
                    .iter()
                    .map(|probe| Self::chain_expires(&chain, probe.link()))
                    .collect();
                if !warming
                    .kept
                    .iter()
                    .zip(&chain_ends)
                    .any(|(kept, &chain_end)| kept.pending.is_some() && chain_end <= now)
                {
                    continue;
                }
                let mut waiting = Vec::new();
                let probes = std::mem::take(&mut warming.kept);
                for (kept, chain_end) in probes.into_iter().zip(chain_ends) {
                    match kept.pending {
                        Some(label) if chain_end <= now => {
                            self.settlements_after_save.push(kept.settle(key, label));
                        }
                        _ => waiting.push(kept),
                    }
                }
                warming.kept = waiting;
                settled.push((key.clone(), agent_id.clone()));
            }
        }
        for (key, agent_id) in &settled {
            changes.agent_row(key, agent_id);
        }
        changes
    }

    /// A gate's hold on a retained recipe. One hold withholds one probe, whatever number of ticks it spans: the
    /// production ticker runs each second, so a five-hour cooldown would otherwise ledger 18,000 withheld probes
    /// for the one it actually withheld. A hold that begins before the due time has withheld nothing yet, so the
    /// skip waits for the due time to pass. The due time is read after the policy refresh, so the pool's current
    /// policy decides whether a probe was due, and a probe already in flight was not withheld.
    pub(crate) fn hold(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        now: i64,
    ) -> Changes {
        let mut changes = Changes::default();
        let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        else {
            return changes;
        };
        let began = warming.reason != reason;
        let counted = warming.hold_counted && !began;
        let was_due = !warming.in_flight && warming.due_at().is_some_and(|at| at <= now);
        // A hold spans every tick it lasts, so only a change is written: a held recipe would otherwise rewrite its
        // sealed prompt at every flush.
        let moved = warming.hold_counted != (counted || was_due);
        warming.hold_counted = counted || was_due;
        if !counted && was_due {
            let state = recipe_state(reason, warming.paused);
            self.skipped.push(Skipped::of(key, warming, state));
        }
        if began {
            changes.append(self.set_reason(sessions, key, agent_id, reason));
        } else if moved {
            changes.agent_row(key, agent_id);
        }
        changes
    }

    /// The probes sent since the last real request, settled under `label` the way a discard settles them, with the
    /// recipe kept: a hold that retains the prompt still writes off what it paid for.
    pub(crate) fn settle_unanswered(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        label: Wasted,
    ) -> Changes {
        let mut changes = Changes::default();
        let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        else {
            return changes;
        };
        let probes = warming.take_unanswered();
        if probes.is_empty() {
            return changes;
        }
        self.settlements
            .extend(probes.into_iter().map(|kept| kept.settle(key, label)));
        changes.agent_row(key, agent_id);
        changes
    }

    /// The entry is dead at its expiry and the lead already places the due time ahead of it, so a probe past it warms
    /// nothing: the recipe ends under `expired` rather than being probed, held or not, its probes written off
    /// (`lapse_probes`), and the next real request re-arms it. A probe already in flight was sent in time and settles
    /// on its own answer, a hold that already ledgered the probe it withheld is not counted again, and the skip takes
    /// the recipe's speed before the lapse drops the recipe. While a request is open on the agent, the one the
    /// observation follows or a compaction, the recipe is held unprobed instead: its completion earns its credit and
    /// decides the probes, and the next tick lapses it. A paused recipe loses its prompt alone (`drop_paused_prompt`).
    /// True when the tick is done with the recipe.
    pub(crate) fn lapse(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
        lapsed: &mut Vec<Skipped>,
    ) -> (Changes, bool) {
        let mut changes = Changes::default();
        let warming = warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
        let Some(expires) = warming
            .expires
            .filter(|expires| !warming.in_flight && now >= *expires)
        else {
            return (changes, false);
        };
        if request_open(&self.pending, warming) {
            return (changes, true);
        }
        if warming.paused {
            tracing::info!(
                key = %key.0,
                session = %key.1,
                agent = %agent_id,
                expires = %phoebis_store::utc_stamp(expires),
                late_ms = now - expires,
                "a paused keepalive recipe's entry expired, so its prompt was dropped"
            );
            changes.append(self.drop_paused_prompt(sessions, key, agent_id, now));
            return (changes, true);
        }
        tracing::info!(
            key = %key.0,
            session = %key.1,
            agent = %agent_id,
            expires = %phoebis_store::utc_stamp(expires),
            late_ms = now - expires,
            "a keepalive recipe lapsed before its probe, so the probe was skipped"
        );
        if !warming.hold_counted {
            // The lapse is what withholds the probe, so it counts under the state the recipe's end below leaves.
            lapsed.push(Skipped::of(
                key,
                warming,
                recipe_state(Reason::Expired, warming.paused),
            ));
        }
        self.lapse_probes(sessions, key, agent_id, now);
        changes.append(self.end_observation(
            sessions,
            key,
            agent_id,
            Some(Wasted::Expired),
            Unanswered::Keep,
        ));
        changes.append(self.set_reason(sessions, key, agent_id, Reason::Expired));
        (changes, true)
    }

    /// The lapse of a paused recipe. The pause withheld every probe and holds until the next real request, so the agent
    /// keeps its pause bit and its reason for the view's `paused`, and only the prompt the cold entry made worthless
    /// leaves, with the probes it paid for written off under `expired` (`lapse_probes`). No probe was due, so no skip
    /// is ledgered.
    fn drop_paused_prompt(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
    ) -> Changes {
        let mut changes = Changes::default();
        changes.agent_row(key, agent_id);
        let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        else {
            return changes;
        };
        drop_current(warming, &mut self.pending, &mut self.retained);
        warming.expires = None;
        warming.last_stop = Some(Wasted::Expired);
        self.lapse_probes(sessions, key, agent_id, now);
        changes
    }

    /// A lapse's write-off of the agent's probes under `expired`, judged as the lapse sweep judges them: a probe whose
    /// chain's last entry (`chain_expires` over the kept and the unanswered probes) ended at or before `now` settles
    /// here, and the rest move into `kept` under the label, for the sweep to settle at their chain's end or a turn
    /// inside it to read. The recipe a failed turn restored can lapse while a probe the failed turn's own recipe sent
    /// keeps the chain alive past it.
    fn lapse_probes(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
    ) {
        let Some(warming) = agent_ref(&sessions.entries, key, agent_id)
            .map(|_| warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD))
        else {
            return;
        };
        let unanswered = warming.take_unanswered();
        let probes: Vec<Kept> = std::mem::take(&mut warming.kept)
            .into_iter()
            .chain(unanswered)
            .collect();
        let links: Vec<ChainLink> = probes.iter().map(Kept::link).collect();
        let chain_ends: Vec<i64> = links
            .iter()
            .map(|&probe| Self::chain_expires(&links, probe))
            .collect();
        for (kept, chain_end) in probes.into_iter().zip(chain_ends) {
            if chain_end <= now {
                self.settlements.push(kept.settle(key, Wasted::Expired));
            } else {
                warming.kept.push(Kept {
                    pending: Some(Wasted::Expired),
                    ..kept
                });
            }
        }
    }

    /// Every model name a live recipe compares against the router's rows: the recipe's own, and the one its
    /// open observation took from the recipe before it. An open compaction's own and the one it took count too, since
    /// its completion compares them while the recipe it would answer may have ended.
    pub(crate) fn retained_models(&self, sessions: &SessionsState) -> Vec<String> {
        let mut models = BTreeSet::new();
        for (key, session) in &sessions.entries {
            for agent_id in session.agents.keys() {
                if !sessions.live(key, agent_id) {
                    continue;
                }
                let warming = warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
                if let Some(observation) = &warming.current {
                    models.insert(warming.canonical_model.clone());
                    if let Some(prior) = &observation.prior {
                        models.insert(prior.model.clone());
                    }
                }
                if let Some(compaction) = &warming.compaction {
                    models.insert(compaction.canonical_model.clone());
                    if let Some(prior) = &compaction.prior {
                        models.insert(prior.model.clone());
                    }
                }
            }
        }
        models.into_iter().collect()
    }

    /// A resolution that landed after a recipe was retained moved the router's pins and model cooldowns to the
    /// dated id. The recipe follows, so the gate finds them and the next request's credit compares like names.
    pub(crate) fn follow_resolutions(
        &mut self,
        sessions: &SessionsState,
        canonical: &HashMap<String, String>,
    ) -> Changes {
        let mut changes = Changes::default();
        let mut followed: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in sessions.entries.iter() {
            for agent_id in session.agents.keys() {
                let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
                if warming.follow_resolutions(canonical) {
                    followed.push((key.clone(), agent_id.clone()));
                }
            }
        }
        for (key, agent_id) in followed {
            changes.agent_row(&key, &agent_id);
        }
        changes
    }

    /// The cheap half of what a tick needs to decide before it asks the router: whether any live agent still
    /// holds a recipe. An idle session, or one whose companion has sent nothing real yet, retains nothing for
    /// the gate to read, so the tick that finds none needs no fresher view than the one it already has.
    pub(crate) fn anything_retained(&self, sessions: &SessionsState) -> bool {
        sessions.entries.iter().any(|(key, session)| {
            session.agents.keys().any(|agent_id| {
                sessions.live(key, agent_id)
                    && warming_ref(&self.agents, key, agent_id)
                        .expect(AGENT_HELD)
                        .current
                        .is_some()
            })
        })
    }

    /// Makes room under `cap` for a recipe of `size` by evicting retained recipes: unattended sessions' agents
    /// first, then attended sessions' idle agents, each oldest activity first. An agent with a probe in flight or a
    /// request still open is never evicted. When even every candidate would not make room nothing is evicted and
    /// the answer is false, so the refusal that follows costs no recipe.
    pub(crate) fn evict_for(
        &mut self,
        sessions: &SessionsState,
        cap: usize,
        size: usize,
    ) -> (Changes, bool) {
        let mut changes = Changes::default();
        let mut candidates: Vec<(bool, i64, SessionKey, String, usize)> = Vec::new();
        for (key, session) in &sessions.entries {
            for (agent_id, agent) in &session.agents {
                let warming = warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
                let Some(observation) = warming.current.as_ref() else {
                    continue;
                };
                if (!session.attended || agent.state == AgentState::Idle)
                    && !warming.in_flight
                    && !request_open(&self.pending, warming)
                {
                    candidates.push((
                        session.attended,
                        agent.last_activity,
                        key.clone(),
                        agent_id.clone(),
                        observation.recipe.size,
                    ));
                }
            }
        }
        candidates.sort();
        let mut freed = 0;
        let mut evicted = Vec::new();
        for (_, _, key, agent_id, bytes) in candidates {
            if self.retained.saturating_sub(freed) + size <= cap {
                break;
            }
            freed += bytes;
            evicted.push((key, agent_id));
        }
        if self.retained.saturating_sub(freed) + size > cap {
            return (changes, false);
        }
        for (key, agent_id) in evicted {
            changes.append(self.discard(
                sessions,
                &key,
                &agent_id,
                Reason::Evicted,
                Some(Wasted::Evicted),
            ));
        }
        (changes, true)
    }

    /// The Go `observeLocked`: evidence from a ready attempt arms the schedule at the entry's expiry, the pool's
    /// lead and this schedule's jitter taken off by `due_at`. A retry already pending stays in force.
    pub(crate) fn observe_usage(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        usage: ProbeUsage,
    ) -> Changes {
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        let Some(observation) = warming.current.as_mut() else {
            return Changes::default();
        };
        if !observation.ready || usage.evidence() == 0 {
            return Changes::default();
        }
        let previous_ms = warming.ttl_ms;
        let requested_ms = observation
            .dispatched_ms
            .unwrap_or(observation.recipe.ttl_ms);
        let mut ttl_ms = usage.ttl_ms(previous_ms, requested_ms);
        if observation.ttl_ms > 0 && observation.ttl_ms < ttl_ms {
            ttl_ms = observation.ttl_ms;
        }
        // A paused recipe takes nothing from this usage, but the request's credit from a probe sent before the pause
        // still files under the TTL its own write would have taken, so the observation keeps that TTL.
        if warming.paused {
            observation.ttl_ms = ttl_ms;
            return Changes::default();
        }
        observation.expected = usage.evidence();
        warming.ttl_ms = ttl_ms;
        note_ttl_change(
            key,
            agent_id,
            previous_ms,
            warming.ttl_ms,
            requested_ms,
            &usage,
        );
        observation.ttl_ms = warming.ttl_ms;
        warming.expires = Some(warming.cache_started + warming.ttl_ms);
        warming.jitter_frac = rand::random::<f64>();
        self.set_reason(sessions, key, agent_id, Reason::Scheduled)
    }
}

/// The one line at a TTL decision, written only when the TTL moved, so a settled recipe's two stream phases and its
/// probes stay silent. The counts are the evidence the decision read, never a body.
pub(crate) fn note_ttl_change(
    key: &SessionKey,
    agent_id: &str,
    previous_ms: i64,
    ttl_ms: i64,
    requested_ms: i64,
    usage: &ProbeUsage,
) {
    if previous_ms == ttl_ms {
        return;
    }
    tracing::debug!(
        key = %key.0,
        session = %key.1,
        agent = %agent_id,
        previous_ms,
        ttl_ms,
        requested_ms,
        read = usage.read,
        write = usage.write,
        five_minutes = usage.five_minutes,
        one_hour = usage.one_hour,
        "a keepalive recipe's TTL changed on cache evidence"
    );
}
