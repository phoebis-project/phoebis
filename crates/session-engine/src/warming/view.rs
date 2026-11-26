//! What the cache warming answers the reads that join both components: the agent counts, a recipe's state word, a
//! session's summary and the warming's counts.

use phoebis_ledger::KeepaliveAgents;

use crate::sessions::state::{
    Agent, AgentState, Session, SessionKey, SessionsState, AGENT_HELD, MAIN_AGENT,
};
use crate::warming::agent::{warming_ref, AgentWarming};
use crate::warming::causes::Reason;
use crate::warming::guard::Guard;
use crate::warming::state::WarmingState;

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

/// The ledger's record of the agent gauge, so the overview's counts and the scrape's are one type.
pub type AgentCounts = KeepaliveAgents;

/// The warming's counts.
#[derive(Debug)]
pub struct WarmingCounts {
    pub bus_gaps: u64,
    /// The last bus gap's instant, `None` before the first.
    pub last_bus_gap_at: Option<i64>,
    pub retained_bytes: usize,
    pub running_probes: usize,
}

impl AgentWarming {
    fn holds_recipe(&self) -> bool {
        self.current.as_ref().is_some_and(|o| o.ready)
    }

    /// The state of the agent's line: a held or a retained word reads `not_retained` where nothing is kept.
    pub(crate) fn view_state(&self) -> RecipeState {
        match recipe_state(self.reason, self.paused) {
            // A refusal at the selection leaves a held word behind with nothing kept, as a failed request that found
            // no recipe armed leaves a retained one, and a held or retained state says the prompt is kept. A request
            // still awaiting its selection or its evidence keeps its observation.
            RecipeState::Held | RecipeState::Retained if self.current.is_none() => {
                RecipeState::NotRetained
            }
            state => state,
        }
    }
}

impl WarmingState {
    pub(crate) fn summary(&self, key: &SessionKey, session: &Session) -> KeepaliveSummary {
        let warming_of =
            |agent_id: &str| warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
        // A session whose own option holds the warming off holds no recipe, so no agent stands for its
        // row. The row reads the cause and the state that a line of the recipes route reads for an agent
        // of such a session.
        if session.cache_warming {
            // Main's recipe stands for the session. Where main holds none, the newest other agent's does, so a
            // session whose main sent auxiliary calls alone while a subagent works still reads a recipe.
            let summary = session
                .agents
                .get(MAIN_AGENT)
                .map(|agent| (agent, warming_of(MAIN_AGENT)))
                .filter(|(_, warming)| warming.holds_recipe())
                .or_else(|| {
                    session
                        .agents
                        .iter()
                        .map(|(id, agent)| (agent, warming_of(id)))
                        .filter(|(_, warming)| warming.holds_recipe())
                        .max_by_key(|(_, warming)| warming.last_real_at)
                });
            KeepaliveSummary {
                cause: summary.map(|(_, warming)| warming.reason),
                last_guard: summary.and_then(|(_, warming)| warming.last_guard),
                next_probe_at: session
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
        }
    }

    /// Every agent the session engine holds, with its session's key and its warming record: the one walk that the
    /// recipes route and the agent gauge read, so the two views visit the same agents.
    pub(crate) fn with_agents<'a>(
        &'a self,
        sessions: &'a SessionsState,
    ) -> impl Iterator<Item = (&'a SessionKey, &'a Agent, &'a AgentWarming)> + 'a {
        sessions.entries.iter().flat_map(move |(key, s)| {
            s.agents
                .iter()
                .map(move |(id, a)| (key, a, self.held(key, id)))
        })
    }

    pub(crate) fn agent_counts(&self, sessions: &SessionsState) -> AgentCounts {
        let mut counts = AgentCounts::default();
        for (_, agent, _) in self
            .with_agents(sessions)
            .filter(|(_, _, warming)| warming.holds_recipe())
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

    pub(crate) fn counts(&self) -> WarmingCounts {
        WarmingCounts {
            bus_gaps: self.bus_gaps,
            last_bus_gap_at: self.last_bus_gap_at,
            retained_bytes: self.retained,
            running_probes: self.running,
        }
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
