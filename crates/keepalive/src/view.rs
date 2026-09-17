//! What the admin routes read from the engine: one row per session, one per recipe, and the counts the
//! overview tiles show. Every reason here is a member of a closed set the UI knows.

use std::sync::Arc;

use phoebis_ledger::{KeepaliveAgents, KeepaliveGaugeSource};

use crate::engine::{Agent, AgentState, Engine, Reason, Wasted, MAIN_AGENT};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewReason {
    AffinityTooShort,
    AwaitingCacheEvidence,
    CapReached,
    ColdProbes,
    Cooled,
    IdleLimit,
    NetNegative,
    OauthUnverified,
    Overage,
    Paused,
    ProbeError,
    QuotaExhausted,
    Reauth,
    Retrying,
    StorageLimit,
    UnsupportedShape,
}

/// The engine's fine-grained reason folded onto the view's vocabulary; `None` means nothing holds the recipe.
pub fn view_reason(reason: Reason) -> Option<ViewReason> {
    use Reason::*;
    Some(match reason {
        AwaitingRequest | AwaitingSelection | Probing | Scheduled => return None,
        AffinityTooShort => ViewReason::AffinityTooShort,
        AwaitingCacheEvidence | NoCacheEvidence | NoProbeCacheEvidence => {
            ViewReason::AwaitingCacheEvidence
        }
        CapReached => ViewReason::CapReached,
        RepeatedColdProbe | PartialCacheCoverage => ViewReason::ColdProbes,
        Cooled => ViewReason::Cooled,
        IdleLimit => ViewReason::IdleLimit,
        NetNegative => ViewReason::NetNegative,
        OauthUnverified => ViewReason::OauthUnverified,
        Overage => ViewReason::Overage,
        Paused | PoolDisabled | KeepaliveOff | OptedOut | KeyOptedOut
        | IneligibleCredential => ViewReason::Paused,
        InvalidProbeUsage
        | MissingAccount
        | ProbeAuthenticationError
        | ProbeRetryLimit
        | ProbeTransportError
        | UnexpectedOutput
        | UnsupportedProbe
        | RequestCanceled
        | RequestFailed
        | RequestRejected => ViewReason::ProbeError,
        ProbeQuotaExhausted => ViewReason::QuotaExhausted,
        Reauth => ViewReason::Reauth,
        RetryableProbeError | SlowProbe => ViewReason::Retrying,
        StorageLimit | RequestLimit | RequestTooLarge | HeadersTooLarge => ViewReason::StorageLimit,
        FixedThinking | ForcedTool | InvalidCacheControl | InvalidModel | InvalidRequest
        | StructuredOutput | UnsupportedShape | OwnerMismatch | AgentUnknown | AgentReleased
        | BusGap | InstanceReplaced | LeaseExpired | Released | Reset | SessionEnded
        | SessionExpired => ViewReason::UnsupportedShape,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeState {
    Armed,
    Held,
    Probing,
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
    pub account_id: Option<String>,
    pub agent_id: String,
    pub key_id: String,
    pub last_request_at: Option<i64>,
    pub model: String,
    pub next_probe_at: Option<i64>,
    pub pool_id: String,
    pub reason: Option<ViewReason>,
    pub session_id: String,
    pub state: RecipeState,
    pub ttl_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct KeepaliveSummary {
    pub next_probe_at: Option<i64>,
    pub state: Option<AgentState>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SessionView {
    pub agents: usize,
    pub companion: CompanionState,
    pub companion_since: Option<i64>,
    pub key_id: String,
    pub keepalive: KeepaliveSummary,
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

fn recipe_of(agent: &Agent, key_id: &str, session_id: &str) -> RecipeView {
    let reason = view_reason(agent.reason);
    RecipeView {
        account_id: agent.account.clone(),
        agent_id: agent.id.clone(),
        key_id: key_id.into(),
        last_request_at: agent.last_real_at,
        model: agent.canonical_model.clone(),
        next_probe_at: agent.due_at(),
        pool_id: agent.pool_id.clone(),
        reason,
        session_id: session_id.into(),
        state: if agent.in_flight {
            RecipeState::Probing
        } else if reason.is_some() {
            RecipeState::Held
        } else {
            RecipeState::Armed
        },
        ttl_seconds: agent.ttl_ms / 1_000,
    }
}

impl Engine {
    pub fn sessions(&self) -> Vec<SessionView> {
        self.expire();
        let inner = self.lock();
        inner
            .sessions
            .values()
            .map(|s| {
                let main = s.agents.get(MAIN_AGENT).filter(|a| holds_recipe(a));
                SessionView {
                    agents: s.agents.len(),
                    companion: if s.attended {
                        CompanionState::Reporting
                    } else {
                        CompanionState::Silent
                    },
                    companion_since: (!s.attended).then_some(s.last_report_at),
                    key_id: s.key_id.clone(),
                    keepalive: KeepaliveSummary {
                        next_probe_at: s.agents.values().filter_map(|a| a.due_at()).min(),
                        state: main.map(|a| a.state),
                    },
                    session_id: s.session_id.clone(),
                }
            })
            .collect()
    }

    pub fn recipes(&self) -> Vec<RecipeView> {
        self.expire();
        let inner = self.lock();
        inner
            .sessions
            .values()
            .flat_map(|s| {
                s.agents
                    .values()
                    .filter(|a| holds_recipe(a))
                    .map(move |a| recipe_of(a, &s.key_id, &s.session_id))
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

    /// The operator's release: every recipe of the session goes, labelled `released`, and the session stays
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

    /// Every reason the engine can hold an agent under maps onto the view's vocabulary, or is one of the small
    /// set that means nothing is held.
    #[test]
    fn every_reason_maps_onto_the_view_vocabulary_or_none() {
        let unheld = [
            Reason::AwaitingRequest,
            Reason::AwaitingSelection,
            Reason::Probing,
            Reason::Scheduled,
        ];
        for reason in Reason::ALL {
            let mapped = view_reason(*reason);
            assert_eq!(
                mapped.is_none(),
                unheld.contains(reason),
                "{reason:?} -> {mapped:?}"
            );
        }
    }
}
