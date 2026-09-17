//! The engine: every session the companion registered or a request revealed, every agent's recipe, and the
//! counters behind the views. One mutex, as the Go engine had, held for microseconds at a time; nothing under
//! it does I/O.
//!
//! This file grows across several tasks; a field or method with no reader yet belongs to one that hasn't
//! landed, so dead-code warnings are silenced here rather than per item.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::Bus;
use phoebis_ledger::{Ledger, Metrics, Pricing};
use phoebis_oauth::Clock;
use phoebis_relay::Relay;
use phoebis_routing::RouterHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{KeepaliveLimits, KeepalivePolicy};
use crate::recipe::Recipe;
use crate::usage::ProbeUsage;

const LOCK_POISONED: &str = "keepalive engine poisoned by a prior panic";

/// The main conversation's agent, which the idle limits apply to.
pub const MAIN_AGENT: &str = "main";

pub type SessionKey = (String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Busy,
    Idle,
    Parked,
    Released,
    Unknown,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Busy => "busy",
            AgentState::Idle => "idle",
            AgentState::Parked => "parked",
            AgentState::Released => "released",
            AgentState::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<AgentState> {
        Some(match s {
            "busy" => AgentState::Busy,
            "idle" => AgentState::Idle,
            "parked" => AgentState::Parked,
            "released" => AgentState::Released,
            "unknown" => AgentState::Unknown,
            _ => return None,
        })
    }
}

/// Why a recipe stopped while probes it paid for were unanswered; the label the wasted count carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wasted {
    IdleLimit,
    Released,
    Replaced,
    SessionEnded,
}

impl Wasted {
    pub fn as_str(self) -> &'static str {
        match self {
            Wasted::IdleLimit => "idle_limit",
            Wasted::Released => "released",
            Wasted::Replaced => "replaced",
            Wasted::SessionEnded => "session_ended",
        }
    }
}

macro_rules! reasons {
    ($($variant:ident => $text:literal),* $(,)?) => {
        /// Every state an agent's recipe can be in, as the Go engine named them plus the router's own gates.
        /// A reason is a permanent counter key, so nothing mints one at runtime.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Reason { $($variant),* }
        impl Reason {
            pub const ALL: &'static [Reason] = &[$(Reason::$variant),*];
            pub fn as_str(self) -> &'static str {
                match self { $(Reason::$variant => $text),* }
            }
        }
    };
}

reasons! {
    AffinityTooShort => "affinity_too_short",
    AgentReleased => "agent_released",
    AgentUnknown => "agent_unknown",
    AwaitingCacheEvidence => "awaiting_cache_evidence",
    AwaitingRequest => "awaiting_request",
    AwaitingSelection => "awaiting_selection",
    BusGap => "bus_gap",
    CapReached => "cap_reached",
    Cooled => "cooled",
    FixedThinking => "fixed_thinking",
    ForcedTool => "forced_tool",
    HeadersTooLarge => "headers_too_large",
    IdleLimit => "idle_limit",
    IneligibleCredential => "ineligible_credential",
    InstanceReplaced => "instance_replaced",
    InvalidCacheControl => "invalid_cache_control",
    InvalidModel => "invalid_model",
    InvalidProbeUsage => "invalid_probe_usage",
    InvalidRequest => "invalid_request",
    KeepaliveOff => "keepalive_off",
    KeyOptedOut => "key_opted_out",
    LeaseExpired => "lease_expired",
    MissingAccount => "missing_account",
    NetNegative => "net_negative",
    NoCacheEvidence => "no_cache_evidence",
    NoProbeCacheEvidence => "no_probe_cache_evidence",
    OauthUnverified => "oauth_unverified",
    OptedOut => "opted_out",
    Overage => "overage",
    OwnerMismatch => "owner_mismatch",
    PartialCacheCoverage => "partial_cache_coverage",
    Paused => "paused",
    PoolDisabled => "pool_disabled",
    ProbeAuthenticationError => "probe_authentication_error",
    ProbeQuotaExhausted => "probe_quota_exhausted",
    ProbeRetryLimit => "probe_retry_limit",
    ProbeTransportError => "probe_transport_error",
    Probing => "probing",
    Reauth => "reauth",
    Released => "released",
    RepeatedColdProbe => "repeated_cold_probe",
    RequestCanceled => "request_canceled",
    RequestFailed => "request_failed",
    RequestLimit => "request_limit",
    RequestRejected => "request_rejected",
    RequestTooLarge => "request_too_large",
    Reset => "reset",
    RetryableProbeError => "retryable_probe_error",
    Scheduled => "scheduled",
    SessionEnded => "session_ended",
    SessionExpired => "session_expired",
    SlowProbe => "slow_probe",
    StorageLimit => "storage_limit",
    StructuredOutput => "structured_output",
    UnexpectedOutput => "unexpected_output",
    UnsupportedProbe => "unsupported_probe",
    UnsupportedShape => "unsupported_shape",
}

pub struct EngineParts {
    pub bus: Bus,
    pub clock: Clock,
    pub ledger: Ledger,
    pub limits: KeepaliveLimits,
    pub metrics: Arc<Metrics>,
    pub pricing: Arc<Pricing>,
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    pub session_expiry: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub agents: usize,
    pub bus_gaps: u64,
    pub retained_bytes: usize,
    pub running_probes: usize,
    pub sessions: usize,
}

/// One companion registration, or one session a request revealed before any registration.
pub(crate) struct Session {
    pub key_id: String,
    pub session_id: String,
    pub instance: String,
    pub sequence: u64,
    /// When the companion's lease runs out. Past it the session is unattended: its recipes stay and are probed
    /// under the idle limits from `last_real_at`.
    pub lease: i64,
    pub attended: bool,
    /// A `session_end` or a replacement: nothing under this instance is live any more.
    pub ended: bool,
    pub owner_id: Option<String>,
    pub last_report_at: i64,
    pub last_real_at: Option<i64>,
    pub agents: BTreeMap<String, Agent>,
}

pub(crate) struct Agent {
    pub id: String,
    /// The session's key, repeated here so a gate over one agent needs no session lookup.
    pub key_id: String,
    /// Which record this is. A replacement registration builds a new record under the same ids, and a probe
    /// worker that started on the old one must not touch the new one's bookkeeping.
    pub incarnation: u64,
    pub state: AgentState,
    pub generation: u64,
    pub current: Option<Observation>,
    /// When the entry the recipe keeps warm expires; the due time is this less the pool's lead and jitter.
    pub expires: Option<i64>,
    /// The share of the pool's jitter this schedule takes, rolled once per schedule so the due time holds
    /// still between ticks while the pool's tuning may change under it.
    pub jitter_frac: f64,
    /// An outstanding backoff after a failed probe; the due time is the later of it and the expiry's.
    pub retry_at: Option<i64>,
    /// The pool's policy as of the last admission or tick, so a due time never needs the registry.
    pub policy: KeepalivePolicy,
    pub last_activity: i64,
    pub cache_started: i64,
    pub ttl_ms: i64,
    pub idle_ticks: u64,
    pub cold: u32,
    pub failures: u32,
    pub account: Option<String>,
    pub pool_id: String,
    /// The canonical model of the request the recipe came from, as the relay resolved it: the price lookups,
    /// the rollup keys, the cooldown scope, and the views read this, never the caller's spelling in the recipe.
    pub canonical_model: String,
    pub reason: Reason,
    pub paused: bool,
    pub in_flight: bool,
    pub probes: u64,
    pub migrations: u64,
    pub usage: ProbeUsage,
    /// The savings ledger of this agent: the running net the pause rule reads, the grace probes left under a
    /// negative net, the cost of every probe since the last real request (settled as wasted when the recipe
    /// stops), and whether a probe with full coverage ran since that request.
    pub net_usd: f64,
    pub grace_left: u32,
    pub unanswered: Vec<f64>,
    pub covered_since_real: bool,
    pub last_real_at: Option<i64>,
    /// The label of the last discard that had one, so a probe result arriving after the recipe stopped settles
    /// under the stop that orphaned it.
    pub last_stop: Option<Wasted>,
}

impl Agent {
    pub(crate) fn new(id: &str, key_id: &str, incarnation: u64, now: i64) -> Agent {
        Agent {
            id: id.into(),
            key_id: key_id.into(),
            incarnation,
            state: AgentState::Busy,
            generation: 0,
            current: None,
            expires: None,
            jitter_frac: 0.0,
            retry_at: None,
            policy: KeepalivePolicy::default(),
            last_activity: now,
            cache_started: now,
            ttl_ms: 0,
            idle_ticks: 0,
            cold: 0,
            failures: 0,
            account: None,
            pool_id: String::new(),
            canonical_model: String::new(),
            reason: Reason::AwaitingRequest,
            paused: false,
            in_flight: false,
            probes: 0,
            migrations: 0,
            usage: ProbeUsage::default(),
            net_usd: 0.0,
            grace_left: 0,
            unanswered: Vec::new(),
            covered_since_real: false,
            last_real_at: None,
            last_stop: None,
        }
    }

    /// When the next probe is due: the entry's expiry less the pool's lead and this schedule's share of the
    /// jitter, or the outstanding backoff, whichever is later. `None` while nothing is armed.
    pub(crate) fn due_at(&self) -> Option<i64> {
        if self.paused {
            return None;
        }
        let from_expiry = self.expires.map(|expires| {
            let jitter = (self.policy.jitter_ms(self.ttl_ms) as f64 * self.jitter_frac) as i64;
            expires - self.policy.lead_ms(self.ttl_ms) - jitter
        });
        match (from_expiry, self.retry_at) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(i64::MIN).max(b.unwrap_or(i64::MIN))),
        }
    }

    /// Whether a probe is scheduled at all, which the Go engine expressed as a non-zero next refresh time.
    pub(crate) fn armed(&self) -> bool {
        self.due_at().is_some()
    }

    pub(crate) fn pause(&mut self) {
        self.paused = true;
        self.expires = None;
        self.retry_at = None;
    }
}

/// What the agent's previous recipe had proved when this request arrived, taken at admission and consumed with
/// the observation. The kept-warm credit reads nothing else: a probe that runs while this request is open, or a
/// TTL this request's own evidence shortens, cannot reach into it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PriorCoverage {
    pub covered: bool,
    pub model: String,
    pub real_at: i64,
    pub ttl_ms: i64,
}

/// One real request the engine is following from its observation to its completion.
pub(crate) struct Observation {
    pub id: String,
    pub generation: u64,
    pub attempt: u8,
    pub started: i64,
    pub recipe: Recipe,
    pub ready: bool,
    pub expected: u64,
    pub ttl_ms: i64,
    pub prior: Option<PriorCoverage>,
}

/// Probes a stopped recipe leaves unanswered, with the label the stop gives them. Queued under the lock and
/// written to the ledger by the engine once the lock is released.
pub(crate) struct Settlement {
    pub key: SessionKey,
    pub agent_id: String,
    pub pool_id: String,
    pub canonical_model: String,
    pub account: Option<String>,
    pub label: Wasted,
    pub costs: Vec<f64>,
}

pub(crate) struct Inner {
    pub sessions: BTreeMap<SessionKey, Session>,
    /// Request id to the agent following it.
    pub pending: HashMap<String, (SessionKey, String)>,
    pub reasons: BTreeMap<Reason, u64>,
    pub retained: usize,
    pub agents: usize,
    pub running: usize,
    pub closed: bool,
    /// Probe cost per pool for the current UTC day, and the day it was summed for.
    pub spent_today: BTreeMap<String, f64>,
    pub spent_day: i64,
    pub settlements: Vec<Settlement>,
    /// Due probes a gate withheld this tick, ledgered as skipped.
    pub skipped: Vec<(SessionKey, String)>,
    /// Bus gaps seen by the observer; each one discarded every observation that was still pending.
    pub bus_gaps: u64,
    pub next_incarnation: u64,
}

pub struct Engine {
    pub(crate) inner: Mutex<Inner>,
    pub(crate) parts: EngineParts,
    pub(crate) cancel: CancellationToken,
    pub(crate) workers: TaskTracker,
}

impl Engine {
    pub fn new(parts: EngineParts) -> Arc<Engine> {
        Arc::new(Engine {
            inner: Mutex::new(Inner {
                sessions: BTreeMap::new(),
                pending: HashMap::new(),
                reasons: BTreeMap::new(),
                retained: 0,
                agents: 0,
                running: 0,
                closed: false,
                spent_today: BTreeMap::new(),
                spent_day: 0,
                settlements: Vec::new(),
                skipped: Vec::new(),
                bus_gaps: 0,
                next_incarnation: 0,
            }),
            parts,
            cancel: CancellationToken::new(),
            workers: TaskTracker::new(),
        })
    }

    pub fn limits(&self) -> &KeepaliveLimits {
        &self.parts.limits
    }

    pub fn now(&self) -> i64 {
        (self.parts.clock)()
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect(LOCK_POISONED)
    }

    pub fn reasons(&self) -> BTreeMap<Reason, u64> {
        self.lock().reasons.clone()
    }

    pub fn stats(&self) -> EngineStats {
        let inner = self.lock();
        EngineStats {
            agents: inner.agents,
            bus_gaps: inner.bus_gaps,
            retained_bytes: inner.retained,
            running_probes: inner.running,
            sessions: inner.sessions.len(),
        }
    }

    /// The lease and expiry sweep. Every entry point runs it first, so a session is never read as live past
    /// its lease by a caller that happened to arrive before the ticker.
    pub fn expire(&self) {
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        let mut inner = self.lock();
        inner.expire(now, expiry);
    }

    pub fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        let keys: Vec<SessionKey> = inner.sessions.keys().cloned().collect();
        for key in keys {
            let agents: Vec<String> = inner.sessions[&key].agents.keys().cloned().collect();
            for agent in agents {
                inner.discard(&key, &agent, Reason::Reset, None);
            }
        }
        inner.sessions.clear();
        inner.pending.clear();
        inner.agents = 0;
        inner.retained = 0;
        self.cancel.cancel();
    }
}

impl Inner {
    /// The one writer of an agent's reason, so the counter behind the views never disagrees with the reason
    /// they show.
    pub(crate) fn set_reason(&mut self, key: &SessionKey, agent_id: &str, reason: Reason) {
        if let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        {
            agent.reason = reason;
        }
        *self.reasons.entry(reason).or_default() += 1;
    }

    /// Drops the agent's recipe and everything following it, and moves the agent to a new generation so no
    /// late result about the old one lands. `wasted` labels the probes the recipe leaves unanswered.
    pub(crate) fn discard(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
        wasted: Option<Wasted>,
    ) {
        let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        else {
            return;
        };
        if let Some(observation) = agent.current.take() {
            self.pending.remove(&observation.id);
            self.retained -= observation.recipe.size;
        }
        agent.expires = None;
        agent.retry_at = None;
        agent.generation += 1;
        agent.paused = false;
        agent.last_stop = wasted;
        let costs = std::mem::take(&mut agent.unanswered);
        if let (Some(label), false) = (wasted, costs.is_empty()) {
            let settlement = Settlement {
                key: key.clone(),
                agent_id: agent_id.to_string(),
                pool_id: agent.pool_id.clone(),
                canonical_model: agent.canonical_model.clone(),
                account: agent.account.clone(),
                label,
                costs,
            };
            self.settlements.push(settlement);
        }
        self.set_reason(key, agent_id, reason);
    }

    pub(crate) fn incarnation(&mut self) -> u64 {
        self.next_incarnation += 1;
        self.next_incarnation
    }

    /// Whether a result about `agent` may still land: the session is not ended, the agent is registered, and
    /// its state admits traffic. An unattended session is live; the idle limits decide what happens to it.
    pub(crate) fn live(&self, key: &SessionKey, agent_id: &str) -> bool {
        !self.closed
            && self.sessions.get(key).is_some_and(|s| {
                !s.ended
                    && s.agents.get(agent_id).is_some_and(|a| {
                        !matches!(a.state, AgentState::Released | AgentState::Unknown)
                    })
            })
    }

    pub(crate) fn expire(&mut self, now: i64, expiry_ms: i64) {
        let lapsed: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.attended && !s.ended && now >= s.lease)
            .map(|(k, _)| k.clone())
            .collect();
        for key in lapsed {
            let agents: Vec<String> = self.sessions[&key].agents.keys().cloned().collect();
            // The recipes stay: a silent companion is not an ended session, and the idle limits bound what the
            // engine spends on it. The reason counts the lapse once per agent, as the Go engine counted it.
            for agent in agents {
                self.set_reason(&key, &agent, Reason::LeaseExpired);
            }
            if let Some(session) = self.sessions.get_mut(&key) {
                session.attended = false;
            }
        }
        let expired: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| now - s.last_real_at.unwrap_or(0).max(s.last_report_at) >= expiry_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            let agents: Vec<String> = self.sessions[&key].agents.keys().cloned().collect();
            for agent in &agents {
                self.discard(
                    &key,
                    agent,
                    Reason::SessionExpired,
                    Some(Wasted::SessionEnded),
                );
            }
            self.agents -= agents.len();
            self.sessions.remove(&key);
        }
    }
}
