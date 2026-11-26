//! What the admin routes read from the engine: one row per session, one per agent that has sent a real request, and
//! the counts the overview tiles show. Every word here is a member of a closed set the UI knows.

use std::collections::BTreeMap;
use std::sync::Arc;

use phoebis_ledger::KeepaliveGaugeSource;

use crate::engine::Engine;
use crate::sessions::state::{Agent, AgentState, Park, ParkKind};
use crate::sessions::view::CompanionState;
use crate::warming::agent::AgentWarming;
use crate::warming::causes::Reason;
use crate::warming::guard::Guard;
use crate::warming::view::{AgentCounts, KeepaliveSummary, RecipeState};

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

#[derive(Clone, Debug, serde::Serialize)]
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
        state: warming.view_state(),
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
            .map(|(key, s)| SessionView {
                agents: s.agents.len(),
                companion: s.companion(),
                companion_since: s.companion_since(),
                key_id: key.0.clone(),
                key_generation: s.key_generation,
                keepalive: inner.warming.summary(key, s),
                lifecycles: s
                    .agents
                    .iter()
                    .map(|(id, a)| (id.clone(), Lifecycle::of(a, inner.warming.held(key, id))))
                    .collect(),
                presence_age_seconds: s.presence_age_seconds(now),
                session_id: key.1.clone(),
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
            .warming
            .with_agents(&inner.sessions)
            .filter(|(_, _, warming)| warming.last_real_at.is_some())
            .map(|(key, a, warming)| recipe_of(a, warming, &key.0, &key.1, now))
            .collect()
    }

    pub fn agent_counts(&self) -> AgentCounts {
        let inner = self.lock();
        inner.warming.agent_counts(&inner.sessions)
    }

    /// A closure the metrics renderer can call on every scrape without the ledger crate depending on this
    /// one: it reads the engine's own lock fresh each time rather than pushing a stale snapshot.
    pub fn gauge_source(self: &Arc<Self>) -> KeepaliveGaugeSource {
        let engine = self.clone();
        Arc::new(move || engine.agent_counts())
    }
}
