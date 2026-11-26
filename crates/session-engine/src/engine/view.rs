//! What the admin routes read from the engine: one row per session, one per agent that has sent a real request, and
//! the counts the overview tiles show. Every word here is a member of a closed set the UI knows.

use std::collections::BTreeMap;
use std::sync::Arc;

use phoebis_ledger::{KeepaliveAgents, KeepaliveGaugeSource};

use crate::engine::Engine;
use crate::sessions::state::{Agent, AgentState, Park, ParkKind, AGENT_HELD, MAIN_AGENT};
use crate::warming::agent::{warming_ref, AgentWarming};
use crate::warming::causes::Reason;
use crate::warming::guard::Guard;

/// The Sessions row's one word for an agent's recipe, read off the engine's reason beside its pause bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeState {
    Ended,
    Held,
    NotRetained,
    Paused,
    Retained,
}

impl RecipeState {
    /// The word serde writes, for a label that is not serialized through serde.
    pub fn as_str(self) -> &'static str {
        match self {
            RecipeState::Ended => "ended",
            RecipeState::Held => "held",
            RecipeState::NotRetained => "not_retained",
            RecipeState::Paused => "paused",
            RecipeState::Retained => "retained",
        }
    }
}

/// The state a recipe under `reason` reads in. `held` clears on its own or by an operator, `paused` is the engine's
/// own pause until the next real request, `ended` is a discarded recipe whose agent stays, `not_retained` a request
/// the engine did not keep, and `retained` a recipe armed, probing or waiting for its evidence. A held or retained
/// word reads `paused` while the pause bit holds: a pause clears the schedule, and a hold a gate places on a paused
/// recipe at a later tick re-arms nothing.
pub fn recipe_state(reason: Reason, paused: bool) -> RecipeState {
    use Reason::*;
    let state = match reason {
        // A failed or canceled request leaves the last answered recipe armed, so its word says the prompt is kept.
        AwaitingCacheEvidence
        | AwaitingSelection
        | LeaseExpired
        | Probing
        | RequestCanceled
        | RequestFailed
        | Scheduled => RecipeState::Retained,
        AgentLimit | AwaitingRequest | FixedThinking | ForcedTool | HeadersTooLarge
        | InstanceMismatch | InvalidCacheControl | InvalidModel | InvalidRequest
        | NoCacheEvidence | RequestLimit | RequestTooLarge | SessionLimit | StorageLimit
        | StructuredOutput | UnsupportedShape => RecipeState::NotRetained,
        // A probe's transient failure, and a probe that read no cache, keep the prompt and retry on the ladder until
        // the entry's expiry would come first.
        AffinityTooShort | Budget | Cooled | Credits | Horizon | IneligibleCredential
        | KeepaliveOff | LedgerBehind | NoProbeCacheEvidence | Overage | Paused | PinMoved
        | PoolDisabled | ProbePoolExhausted | ProbeQuotaExhausted | ProbeTransportError
        | Reauth | RetryableProbeError | SlowProbe | Spent => RecipeState::Held,
        InvalidProbeUsage
        | MissingAccount
        | NoPin
        | PartialCacheCoverage
        | ProbeAuthenticationError
        | ProbeRetryLimit
        | RepeatedColdProbe
        | RequestRejected
        | UnexpectedOutput
        | UnsupportedProbe => RecipeState::Paused,
        AgentReleased | AgentUnknown | BusGap | Compacted | Evicted | Expired
        | ExpiredAtRestart | IdleLimit | InstanceReplaced | KeyOptedOut | OptedOut
        | ParkedLimit | Reset | SessionEnded | SessionExpired | SessionOptedOut => {
            RecipeState::Ended
        }
    };
    match state {
        RecipeState::Held | RecipeState::Retained if paused => RecipeState::Paused,
        state => state,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionState {
    None,
    Reporting,
    Silent,
}

/// An agent's lifecycle as the views word it: the engine's five states, and `Registered` for a record that is
/// busy awaiting a real request since it registered or started again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Busy,
    Idle,
    Parked,
    Registered,
    Released,
    Unknown,
}

impl Lifecycle {
    /// The agent's word, read off its state and its reason together, so every view words one record alike.
    fn of(agent: &Agent, warming: &AgentWarming) -> Lifecycle {
        match agent.state {
            AgentState::Busy if warming.reason == Reason::AwaitingRequest => Lifecycle::Registered,
            AgentState::Busy => Lifecycle::Busy,
            AgentState::Idle => Lifecycle::Idle,
            AgentState::Parked => Lifecycle::Parked,
            AgentState::Released => Lifecycle::Released,
            AgentState::Unknown => Lifecycle::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RecipeView {
    /// The account the agent is pinned to.
    pub account_id: Option<String>,
    pub agent_id: String,
    /// The agent's lifecycle word, `registered` for a record busy awaiting a real request since it registered or
    /// started again.
    pub agent_state: Lifecycle,
    pub cause: Reason,
    pub credited_today: u64,
    pub deadline: Option<i64>,
    pub key_id: String,
    /// The guard's verdict on the last request the agent observed, `null` before the first one. `auxiliary` and
    /// `foreign` both leave the recipe scheduled exactly as `state` and `cause` already say, `compaction` leaves it as
    /// it stood while the compaction is open, and a completed one stops it under `compacted`.
    pub last_guard: Option<Guard>,
    pub last_probe_at: Option<i64>,
    pub last_request_at: Option<i64>,
    pub model: String,
    pub next_probe_at: Option<i64>,
    pub park_kind: Option<ParkKind>,
    pub pool_id: String,
    pub probes_today: u64,
    pub session_id: String,
    pub state: RecipeState,
    pub ttl_seconds: i64,
    pub wake_at: Option<i64>,
}

/// The state word of a session row's summary: the state of the agent whose recipe stands for the session, or `Ended`
/// for a session whose own option holds the cache warming off, which holds no recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryState {
    Busy,
    Ended,
    Idle,
    Parked,
    Released,
    Unknown,
}

impl From<AgentState> for SummaryState {
    fn from(state: AgentState) -> SummaryState {
        match state {
            AgentState::Busy => SummaryState::Busy,
            AgentState::Idle => SummaryState::Idle,
            AgentState::Parked => SummaryState::Parked,
            AgentState::Released => SummaryState::Released,
            AgentState::Unknown => SummaryState::Unknown,
        }
    }
}

/// Main's recipe as the session row reads it, or the newest other agent's where main holds none, every member null
/// while no agent holds one. A session whose own option holds the cache warming off reads the cause
/// `session_opted_out` and the state `ended`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct KeepaliveSummary {
    pub cause: Option<Reason>,
    pub last_guard: Option<Guard>,
    pub next_probe_at: Option<i64>,
    pub state: Option<SummaryState>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SessionView {
    pub agents: usize,
    pub companion: CompanionState,
    pub companion_since: Option<i64>,
    pub key_id: String,
    pub key_generation: i64,
    pub keepalive: KeepaliveSummary,
    /// Every agent record's lifecycle word by agent id, main's among them, so a line reads its own agent's word.
    pub lifecycles: BTreeMap<String, Lifecycle>,
    /// How long ago main's last presence report arrived, in seconds, `null` before the first one.
    pub presence_age_seconds: Option<i64>,
    pub session_id: String,
}

/// The ledger's record of the agent gauge, so the overview's counts and the scrape's are one type.
pub type AgentCounts = KeepaliveAgents;

fn holds_recipe(warming: &AgentWarming) -> bool {
    warming.current.as_ref().is_some_and(|o| o.ready)
}

fn recipe_of(
    agent: &Agent,
    warming: &AgentWarming,
    key_id: &str,
    session_id: &str,
    now: i64,
) -> RecipeView {
    let (probes_today, credited_today) = warming.counts_today(now);
    let (deadline, park_kind, wake_at) = Park::parts(agent.park.map(|held| held.park));
    RecipeView {
        account_id: warming.account.clone(),
        agent_id: agent.id.clone(),
        agent_state: Lifecycle::of(agent, warming),
        cause: warming.reason,
        credited_today,
        deadline,
        key_id: key_id.into(),
        last_guard: warming.last_guard,
        last_probe_at: warming.last_probe_at,
        last_request_at: warming.last_real_at,
        model: warming.canonical_model.clone(),
        next_probe_at: warming.next_probe_at(),
        park_kind,
        pool_id: warming.pool_id.clone(),
        probes_today,
        session_id: session_id.into(),
        state: match recipe_state(warming.reason, warming.paused) {
            // A refusal at the selection leaves a held word behind with nothing kept, as a failed request that found
            // no recipe armed leaves a retained one, and a held or retained state says the prompt is kept. A request
            // still awaiting its selection or its evidence keeps its observation.
            RecipeState::Held | RecipeState::Retained if warming.current.is_none() => {
                RecipeState::NotRetained
            }
            state => state,
        },
        ttl_seconds: warming.ttl_ms / 1_000,
        wake_at,
    }
}

impl Engine {
    pub fn sessions(&self) -> Vec<SessionView> {
        self.expire();
        let now = self.now();
        let inner = self.lock();
        inner
            .sessions
            .entries
            .iter()
            .map(|(key, s)| {
                let warming_of = |agent_id: &str| {
                    warming_ref(&inner.warming.agents, key, agent_id).expect(AGENT_HELD)
                };
                let main = s.agents.get(MAIN_AGENT);
                // Main's recipe stands for the session. Where main holds none, the newest other agent's does, so a
                // session whose main sent auxiliary calls alone while a subagent works still reads a recipe.
                let summary = main
                    .map(|agent| (agent, warming_of(MAIN_AGENT)))
                    .filter(|(_, warming)| holds_recipe(warming))
                    .or_else(|| {
                        s.agents
                            .iter()
                            .map(|(id, agent)| (agent, warming_of(id)))
                            .filter(|(_, warming)| holds_recipe(warming))
                            .max_by_key(|(_, warming)| warming.last_real_at)
                    });
                // An ended session keeps its tombstone for the lifecycle protocol, but its companion is
                // gone: the engine refuses further events under that instance, and the lease sweep leaves an
                // ended session alone, so `attended` stays as the last heartbeat left it.
                let companion = if s.ended {
                    CompanionState::None
                } else if s.attended {
                    CompanionState::Reporting
                } else {
                    CompanionState::Silent
                };
                SessionView {
                    agents: s.agents.len(),
                    companion,
                    // The end and the lapse are both the moment the companion stopped reporting, which is
                    // the last report either way; a reporting companion has no such moment.
                    companion_since: (companion != CompanionState::Reporting)
                        .then_some(s.last_report_at),
                    key_id: key.0.clone(),
                    key_generation: s.key_generation,
                    // A session whose own option holds the warming off holds no recipe, so no agent stands for its
                    // row. The row reads the cause and the state that a line of the recipes route reads for an agent
                    // of such a session.
                    keepalive: if s.cache_warming {
                        KeepaliveSummary {
                            cause: summary.map(|(_, warming)| warming.reason),
                            last_guard: summary.and_then(|(_, warming)| warming.last_guard),
                            next_probe_at: s
                                .agents
                                .keys()
                                .filter_map(|id| warming_of(id).next_probe_at())
                                .min(),
                            state: summary.map(|(agent, _)| agent.state.into()),
                        }
                    } else {
                        KeepaliveSummary {
                            cause: Some(Reason::SessionOptedOut),
                            last_guard: None,
                            next_probe_at: None,
                            state: Some(SummaryState::Ended),
                        }
                    },
                    lifecycles: s
                        .agents
                        .iter()
                        .map(|(id, a)| (id.clone(), Lifecycle::of(a, warming_of(id))))
                        .collect(),
                    presence_age_seconds: main
                        .and_then(|a| a.last_presence_at)
                        .map(|at| (now - at).max(0) / 1_000),
                    session_id: key.1.clone(),
                }
            })
            .collect()
    }

    /// One row per agent that has sent a real request, whether or not it holds a recipe now: an ended or a refused
    /// recipe keeps its row with the cause until the agent's session leaves the engine or, for a released record, until
    /// the record leaves one cache TTL after its release.
    pub fn recipes(&self) -> Vec<RecipeView> {
        self.expire();
        let now = self.now();
        let inner = self.lock();
        let warming_agents = &inner.warming.agents;
        inner
            .sessions
            .entries
            .iter()
            .flat_map(|(key, s)| {
                s.agents
                    .iter()
                    .map(move |(id, a)| {
                        (a, warming_ref(warming_agents, key, id).expect(AGENT_HELD))
                    })
                    .filter(|(_, warming)| warming.last_real_at.is_some())
                    .map(move |(a, warming)| recipe_of(a, warming, &key.0, &key.1, now))
            })
            .collect()
    }

    pub fn agent_counts(&self) -> AgentCounts {
        let inner = self.lock();
        let warming_agents = &inner.warming.agents;
        let mut counts = AgentCounts::default();
        for (agent, _) in inner
            .sessions
            .entries
            .iter()
            .flat_map(|(key, s)| {
                s.agents.iter().map(move |(id, a)| {
                    (a, warming_ref(warming_agents, key, id).expect(AGENT_HELD))
                })
            })
            .filter(|(_, warming)| holds_recipe(warming))
        {
            match agent.state {
                AgentState::Busy => counts.busy += 1,
                AgentState::Idle => counts.idle += 1,
                AgentState::Parked => counts.parked += 1,
                AgentState::Released | AgentState::Unknown => {}
            }
        }
        counts
    }

    /// A closure the metrics renderer can call on every scrape without the ledger crate depending on this
    /// one: it reads the engine's own lock fresh each time rather than pushing a stale snapshot.
    pub fn gauge_source(self: &Arc<Self>) -> KeepaliveGaugeSource {
        let engine = self.clone();
        Arc::new(move || engine.agent_counts())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The match over `Reason` has no wildcard, so a reason the engine gains fails to build until the table places
    /// it. The pause bit moves only a held or retained word, and it moves it to `paused`.
    #[test]
    fn the_pause_bit_moves_only_a_held_or_retained_word_to_paused() {
        let both = |reason| (recipe_state(reason, false), recipe_state(reason, true));
        assert_eq!(
            both(Reason::Cooled),
            (RecipeState::Held, RecipeState::Paused)
        );
        assert_eq!(
            both(Reason::Scheduled),
            (RecipeState::Retained, RecipeState::Paused)
        );
        assert_eq!(
            both(Reason::Expired),
            (RecipeState::Ended, RecipeState::Ended)
        );
        for state in [
            RecipeState::Ended,
            RecipeState::Held,
            RecipeState::NotRetained,
            RecipeState::Paused,
            RecipeState::Retained,
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
    }
}
