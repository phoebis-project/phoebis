//! What the admin routes read from the engine: one row per session, one per agent that has sent a real request, and
//! the counts the overview tiles show. Every word here is a member of a closed set the UI knows.

use std::collections::BTreeMap;
use std::sync::Arc;

use phoebis_ledger::{KeepaliveAgents, KeepaliveGaugeSource};

use crate::engine::{Agent, AgentState, Engine, Guard, ParkKind, Reason, Wasted, MAIN_AGENT};
use crate::events::{err, EventError};

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
        | InvalidCacheControl | InvalidModel | InvalidRequest | NoCacheEvidence | OwnerMismatch
        | RequestLimit | RequestTooLarge | SessionLimit | StorageLimit | StructuredOutput
        | UnsupportedShape => RecipeState::NotRetained,
        // A probe's transient failure, and a probe that read no cache, keep the prompt and retry on the ladder until
        // the entry's expiry would come first.
        AffinityTooShort | Budget | Cooled | Horizon | IneligibleCredential | KeepaliveOff
        | LedgerBehind | NoProbeCacheEvidence | Overage | Paused | PoolDisabled
        | ProbePoolExhausted | ProbeQuotaExhausted | ProbeTransportError | Reauth
        | RetryableProbeError | SlowProbe => RecipeState::Held,
        InvalidProbeUsage
        | MissingAccount
        | PartialCacheCoverage
        | ProbeAuthenticationError
        | ProbeRetryLimit
        | RepeatedColdProbe
        | RequestRejected
        | UnexpectedOutput
        | UnsupportedProbe => RecipeState::Paused,
        AgentReleased | AgentUnknown | BusGap | Compacted | Evicted | Expired
        | ExpiredAtRestart | IdleLimit | InstanceReplaced | KeyOptedOut | OptedOut
        | ParkedLimit | Released | Reset | SessionEnded | SessionExpired => RecipeState::Ended,
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

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RecipeView {
    /// The account the agent is pinned to.
    pub account_id: Option<String>,
    pub agent_id: String,
    /// The mod's lifecycle word for the agent.
    pub agent_state: AgentState,
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

/// Main's recipe as the session row reads it, or the newest other agent's where main holds none, every member null
/// while no agent holds one.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct KeepaliveSummary {
    pub cause: Option<Reason>,
    pub last_guard: Option<Guard>,
    pub next_probe_at: Option<i64>,
    pub state: Option<AgentState>,
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
    pub lifecycles: BTreeMap<String, AgentState>,
    /// How long ago main's last presence report arrived, in seconds, `null` before the first one.
    pub presence_age_seconds: Option<i64>,
    /// True where main's record reads `Busy` with the reason `AwaitingRequest`: a registration with no real request
    /// yet, which the sessions route words `registered`.
    pub registered: bool,
    pub session_id: String,
}

/// The ledger's record of the agent gauge, so the overview's counts and the scrape's are one type.
pub type AgentCounts = KeepaliveAgents;

fn holds_recipe(agent: &Agent) -> bool {
    agent.current.as_ref().is_some_and(|o| o.ready)
}

fn recipe_of(agent: &Agent, key_id: &str, session_id: &str, now: i64) -> RecipeView {
    let (probes_today, credited_today) = agent.counts_today(now);
    RecipeView {
        account_id: agent.account.clone(),
        agent_id: agent.id.clone(),
        agent_state: agent.state,
        cause: agent.reason,
        credited_today,
        deadline: agent.park.and_then(|p| p.deadline),
        key_id: key_id.into(),
        last_guard: agent.last_guard,
        last_probe_at: agent.last_probe_at,
        last_request_at: agent.last_real_at,
        model: agent.canonical_model.clone(),
        next_probe_at: agent.next_probe_at(),
        park_kind: agent.park.map(|p| p.kind),
        pool_id: agent.pool_id.clone(),
        probes_today,
        session_id: session_id.into(),
        state: match recipe_state(agent.reason, agent.paused) {
            // A refusal at the selection leaves a held word behind with nothing kept, as a failed request that found
            // no recipe armed leaves a retained one, and a held or retained state says the prompt is kept. A request
            // still awaiting its selection or its evidence keeps its observation.
            RecipeState::Held | RecipeState::Retained if agent.current.is_none() => {
                RecipeState::NotRetained
            }
            state => state,
        },
        ttl_seconds: agent.ttl_ms / 1_000,
        wake_at: agent.park.and_then(|p| p.wake_at),
    }
}

impl Engine {
    pub fn sessions(&self) -> Vec<SessionView> {
        self.expire();
        let now = self.now();
        let inner = self.lock();
        inner
            .sessions
            .iter()
            .map(|(key, s)| {
                let main = s.agents.get(MAIN_AGENT);
                // Main's recipe stands for the session. Where main holds none, the newest other agent's does, so a
                // session whose main sent auxiliary calls alone while a subagent works still reads a recipe.
                let summary = main.filter(|a| holds_recipe(a)).or_else(|| {
                    s.agents
                        .values()
                        .filter(|a| holds_recipe(a))
                        .max_by_key(|a| a.last_real_at)
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
                    keepalive: KeepaliveSummary {
                        cause: summary.map(|a| a.reason),
                        last_guard: summary.and_then(|a| a.last_guard),
                        next_probe_at: s.agents.values().filter_map(|a| a.next_probe_at()).min(),
                        state: summary.map(|a| a.state),
                    },
                    lifecycles: s
                        .agents
                        .iter()
                        .map(|(id, a)| (id.clone(), a.state))
                        .collect(),
                    presence_age_seconds: main
                        .and_then(|a| a.last_presence_at)
                        .map(|at| (now - at).max(0) / 1_000),
                    registered: main.is_some_and(|a| {
                        a.state == AgentState::Busy && a.reason == Reason::AwaitingRequest
                    }),
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
        inner
            .sessions
            .iter()
            .flat_map(|(key, s)| {
                s.agents
                    .values()
                    .filter(|a| a.last_real_at.is_some())
                    .map(move |a| recipe_of(a, &key.0, &key.1, now))
            })
            .collect()
    }

    pub fn agent_counts(&self) -> AgentCounts {
        let inner = self.lock();
        let mut counts = AgentCounts::default();
        for agent in inner
            .sessions
            .values()
            .flat_map(|s| s.agents.values())
            .filter(|a| holds_recipe(a))
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

    /// The operator's release: every recipe of the session goes, labeled `released`, and the session stays
    /// registered so its companion keeps reporting. Refused as `quiesced` once the shutdown has quiesced the engine,
    /// before any state changes: the final save has read its snapshot by then, and a release it does not carry would
    /// tell the operator a recipe is gone that the next process restores and probes.
    pub fn release(&self, key_id: &str, session_id: &str) -> Result<usize, EventError> {
        let key = (key_id.to_string(), session_id.to_string());
        let released = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(err("quiesced", 503));
            }
            let held = inner.sessions.get(&key).map_or(0, |s| {
                s.agents.values().filter(|a| a.current.is_some()).count()
            });
            inner.discard_session(&key, Reason::Released, Some(Wasted::Released));
            held
        };
        self.flush();
        Ok(released)
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
