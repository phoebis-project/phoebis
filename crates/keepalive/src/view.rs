//! What the admin routes read from the engine: one row per session, one per agent that has sent a real request, and
//! the counts the overview tiles show. Every word here is a member of a closed set the UI knows.

use std::sync::Arc;

use phoebis_ledger::{KeepaliveAgents, KeepaliveGaugeSource};

use crate::engine::{Agent, AgentState, Engine, Guard, ParkKind, Reason, Wasted, MAIN_AGENT};

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
        AwaitingCacheEvidence | AwaitingSelection | LeaseExpired | Probing | Scheduled => {
            RecipeState::Retained
        }
        AwaitingRequest | FixedThinking | ForcedTool | HeadersTooLarge | InvalidCacheControl
        | InvalidModel | InvalidRequest | NoCacheEvidence | NoProbeCacheEvidence
        | OwnerMismatch | RequestLimit | RequestTooLarge | StorageLimit | StructuredOutput
        | UnsupportedShape => RecipeState::NotRetained,
        AffinityTooShort | Budget | Cooled | Horizon | IneligibleCredential | KeepaliveOff
        | LedgerBehind | Overage | Paused | PoolDisabled | Reauth | RetryableProbeError
        | SlowProbe => RecipeState::Held,
        InvalidProbeUsage
        | MissingAccount
        | PartialCacheCoverage
        | ProbeAuthenticationError
        | ProbePoolExhausted
        | ProbeQuotaExhausted
        | ProbeRetryLimit
        | ProbeTransportError
        | RepeatedColdProbe
        | RequestCanceled
        | RequestFailed
        | RequestRejected
        | UnexpectedOutput
        | UnsupportedProbe => RecipeState::Paused,
        AgentReleased | AgentUnknown | BusGap | Evicted | Expired | ExpiredAtRestart
        | IdleLimit | InstanceReplaced | KeyOptedOut | OptedOut | ParkedLimit | Released
        | Reset | SessionEnded | SessionExpired => RecipeState::Ended,
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
    /// The guard's verdict on the last request the agent observed, `null` before the first one. Never a hold:
    /// `auxiliary` and `foreign` both leave the recipe scheduled exactly as `state` and `cause` already say.
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

/// Main's recipe as the session row reads it, every member `null` while main holds none.
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
    /// How long ago main's last presence report arrived, in seconds, `null` before the first one.
    pub presence_age_seconds: Option<i64>,
    pub session_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct AgentCounts {
    pub busy: u64,
    pub idle: u64,
    pub parked: u64,
}

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
        deadline: agent.deadline,
        key_id: key_id.into(),
        last_guard: agent.last_guard,
        last_probe_at: agent.last_probe_at,
        last_request_at: agent.last_real_at,
        model: agent.canonical_model.clone(),
        next_probe_at: agent.next_probe_at(),
        park_kind: agent.park_kind,
        pool_id: agent.pool_id.clone(),
        probes_today,
        session_id: session_id.into(),
        state: match recipe_state(agent.reason, agent.paused) {
            // A refusal at the selection leaves a held word behind with nothing kept, and a held or retained state
            // says the prompt is kept. A request still awaiting its selection or its evidence keeps its observation.
            RecipeState::Held | RecipeState::Retained if agent.current.is_none() => {
                RecipeState::NotRetained
            }
            state => state,
        },
        ttl_seconds: agent.ttl_ms / 1_000,
        wake_at: agent.wake_at,
    }
}

impl Engine {
    pub fn sessions(&self) -> Vec<SessionView> {
        self.expire();
        let now = self.now();
        let inner = self.lock();
        inner
            .sessions
            .values()
            .map(|s| {
                let main = s.agents.get(MAIN_AGENT).filter(|a| holds_recipe(a));
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
                    key_id: s.key_id.clone(),
                    key_generation: s.key_generation,
                    keepalive: KeepaliveSummary {
                        cause: main.map(|a| a.reason),
                        last_guard: main.and_then(|a| a.last_guard),
                        next_probe_at: s.agents.values().filter_map(|a| a.next_probe_at()).min(),
                        state: main.map(|a| a.state),
                    },
                    presence_age_seconds: s
                        .agents
                        .get(MAIN_AGENT)
                        .and_then(|a| a.last_presence_at)
                        .map(|at| (now - at).max(0) / 1_000),
                    session_id: s.session_id.clone(),
                }
            })
            .collect()
    }

    /// One row per agent that has sent a real request, whether or not it holds a recipe now: an ended or a refused
    /// recipe keeps its row with the cause until the agent's session leaves the engine.
    pub fn recipes(&self) -> Vec<RecipeView> {
        self.expire();
        let now = self.now();
        let inner = self.lock();
        inner
            .sessions
            .values()
            .flat_map(|s| {
                s.agents
                    .values()
                    .filter(|a| a.last_real_at.is_some())
                    .map(move |a| recipe_of(a, &s.key_id, &s.session_id, now))
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
    /// registered so its companion keeps reporting.
    pub fn release(&self, key_id: &str, session_id: &str) -> usize {
        let key = (key_id.to_string(), session_id.to_string());
        let released = {
            let mut inner = self.lock();
            let agents: Vec<String> = inner
                .sessions
                .get(&key)
                .map(|s| s.agents.keys().cloned().collect())
                .unwrap_or_default();
            let held = agents
                .iter()
                .filter(|a| inner.sessions[&key].agents[*a].current.is_some())
                .count();
            for agent in &agents {
                inner.discard(&key, agent, Reason::Released, Some(Wasted::Released));
            }
            held
        };
        self.flush();
        released
    }

    /// A closure the metrics renderer can call on every scrape without the ledger crate depending on this
    /// one: it reads the engine's own lock fresh each time rather than pushing a stale snapshot.
    pub fn gauge_source(self: &Arc<Self>) -> KeepaliveGaugeSource {
        let engine = self.clone();
        Arc::new(move || {
            let counts = engine.agent_counts();
            KeepaliveAgents {
                busy: counts.busy,
                idle: counts.idle,
                parked: counts.parked,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The match over `Reason` has no wildcard, so a reason the engine gains fails to build until the table places
    /// it. This pins the pause bit's rule over every reason: only a held or retained word moves, and it moves to
    /// `paused`.
    #[test]
    fn every_reason_maps_onto_one_of_the_five_states() {
        for reason in Reason::ALL {
            let state = recipe_state(*reason, false);
            let paused = recipe_state(*reason, true);
            match state {
                RecipeState::Held | RecipeState::Retained => {
                    assert_eq!(paused, RecipeState::Paused, "{reason:?}")
                }
                _ => assert_eq!(paused, state, "{reason:?}"),
            }
        }
        let mut counts = std::collections::BTreeMap::new();
        for reason in Reason::ALL {
            *counts
                .entry(recipe_state(*reason, false).as_str())
                .or_insert(0) += 1;
        }
        assert_eq!(
            counts.into_iter().collect::<Vec<_>>(),
            [
                ("ended", 15),
                ("held", 13),
                ("not_retained", 15),
                ("paused", 14),
                ("retained", 5)
            ]
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
