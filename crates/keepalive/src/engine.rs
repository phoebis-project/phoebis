//! The engine: every session the companion registered or a request revealed, every agent's recipe, and the
//! counters behind the views. One mutex, as the Go engine had, held for microseconds at a time; nothing under
//! it does I/O.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{
    spawn_subscriber, AccountSelected, Bus, Event as BusEvent, LifecycleSender, Outcome, Phase,
    RequestComplete, RequestObserved, ResponseChunk, ResponseHead, SessionLifecycle,
};
use phoebis_ledger::{Ledger, Metrics, PlanFactors, PlanWindows, Pricing};
use phoebis_oauth::Clock;
use phoebis_relay::Relay;
use phoebis_routing::{RegistrySnapshot, RouterHandle, FIVE_HOUR};
use phoebis_store::{AccountKind, AccountState, FitRow, OveragePolicy, Store};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{KeepaliveLimits, KeepalivePolicy, MIN_AFFINITY_SECONDS};
use crate::probe::{run_job, wasted_label, ProbeJob};
use crate::recipe::{make_recipe, Recipe};
use crate::savings::gross_saving;
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
    ProbePoolExhausted => "probe_pool_exhausted",
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
    SubagentsOff => "subagents_off",
    UnexpectedOutput => "unexpected_output",
    UnsupportedProbe => "unsupported_probe",
    UnsupportedShape => "unsupported_shape",
}

/// What an observation does to the agent's recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Guard {
    Auxiliary,
    Foreign,
    Rearm,
    Restart,
}

/// The hijack guard: a class names the turn directly; without one, the request's own chain against the armed
/// recipe's two hashes decides, the same fallback the routing crate's `warmth_of` applies when a request
/// carries no chain at all. `recipe` is the armed recipe's `(static_hash, tail_hash)`, `None` for a first
/// observation with nothing to compare against.
pub fn guard(
    request_class: Option<&str>,
    static_hash: Option<&str>,
    boundaries: &[String],
    recipe: Option<(&str, &str)>,
    model_matches: bool,
) -> Guard {
    if let Some(class) = request_class {
        return if phoebis_routing::REFRESHING_CLASSES.contains(&class) {
            Guard::Rearm
        } else {
            Guard::Auxiliary
        };
    }
    let Some((recipe_static, recipe_tail)) = recipe else {
        return Guard::Rearm;
    };
    if static_hash.is_none() && boundaries.is_empty() {
        return Guard::Rearm;
    }
    if boundaries.iter().any(|boundary| boundary == recipe_tail) {
        return Guard::Rearm;
    }
    if static_hash == Some(recipe_static) && model_matches {
        return Guard::Restart;
    }
    Guard::Foreign
}

/// The routing crate's unit selector, so the keepalive and the shedding round never disagree on a pin's unit.
pub type SavingsUnit = phoebis_routing::BurnUnit;

/// How the attempt that served a probe was paid for, captured when it was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Funding {
    ApiKey,
    Overage,
    Plan,
}

impl Funding {
    pub fn of(kind: AccountKind, in_overage: bool) -> Funding {
        match (kind, in_overage) {
            (AccountKind::Oauth, false) => Funding::Plan,
            (AccountKind::Oauth, true) => Funding::Overage,
            (AccountKind::ApiKey, _) => Funding::ApiKey,
        }
    }

    /// `Plan` is meter tokens, the other two dollars: `BurnUnit::of`'s rule.
    pub fn unit(self) -> SavingsUnit {
        match self {
            Funding::Plan => SavingsUnit::MeterTokens,
            Funding::ApiKey | Funding::Overage => SavingsUnit::Usd,
        }
    }
}

/// The two balances of an agent; a settlement lands in the unit of the attempt it settles.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Net {
    pub meter_tokens: f64,
    pub usd: f64,
}

impl Net {
    pub fn in_unit(&self, unit: SavingsUnit) -> f64 {
        match unit {
            SavingsUnit::MeterTokens => self.meter_tokens,
            SavingsUnit::Usd => self.usd,
        }
    }

    pub(crate) fn add(&mut self, unit: SavingsUnit, delta: f64) {
        match unit {
            SavingsUnit::MeterTokens => self.meter_tokens += delta,
            SavingsUnit::Usd => self.usd += delta,
        }
    }
}

pub struct EngineParts {
    pub bus: Bus,
    pub clock: Clock,
    /// The overage predicate's fallback: how long evidence naming no reset stands past its observation, the
    /// same value the relay's `Limits` gets, so the probe gate and the selector read one account the same way.
    pub cooldown_fallback: Duration,
    pub ledger: Ledger,
    /// Where the companion's reports reach the alert inputs; `None` when nothing is listening.
    pub lifecycle: Option<LifecycleSender>,
    pub limits: KeepaliveLimits,
    pub metrics: Arc<Metrics>,
    /// The shipped meter-token weights, for a probe or a kept-warm saving on a plan-funded attempt.
    pub plan: Arc<PlanFactors>,
    pub pricing: Arc<Pricing>,
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    pub session_expiry: Duration,
    /// The registry's own store, read for a subscription account's tier: the plan-side cap's fallback when the
    /// router has not yet fitted the account's window.
    pub store: Arc<Store>,
    /// The shipped window sizes and peak band, for the plan-side probe cap.
    pub windows: Arc<PlanWindows>,
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
    /// The generation of the key that registered the session, so a key id created again reaches none of the
    /// previous holder's sessions: the companion identity is the pair, never the id alone.
    pub key_generation: i64,
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
    /// The generation of the key the recipe was retained under, off the observed request; a probe's and a
    /// settlement's rows file under it.
    pub key_generation: i64,
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
    /// The guard's verdict on the last request the agent observed, `None` before the first one. Never `reason`
    /// itself: an auxiliary or a foreign request never reaches `reason`, which stays the recipe's own gating
    /// state, so a hold's `began` check, the completion's gate-reason match, and the view's armed-or-held state
    /// all read it undisturbed.
    pub last_guard: Option<Guard>,
    /// Whether the current hold has already ledgered the probe it withheld. A hold can begin before its
    /// probe is due, so the skip is counted when the due time passes rather than where the hold started.
    pub hold_counted: bool,
    pub paused: bool,
    pub in_flight: bool,
    pub probes: u64,
    pub migrations: u64,
    pub usage: ProbeUsage,
    /// The savings ledger of this agent: the running net the pause rule reads, the grace probes left under a
    /// negative net, the cost of every probe since the last real request (settled as wasted when the recipe
    /// stops), and whether a probe with full coverage ran since that request.
    pub net: Net,
    pub grace_left: u32,
    pub unanswered: Vec<f64>,
    pub covered_since_real: bool,
    /// The account the covering probe warmed. The credit binds to it: a later request that fails over reads
    /// a prefix ordinary traffic cached on the other account, which no probe of this recipe paid for.
    pub covered_account: Option<String>,
    /// The unit the covering probe's own cost settled in, so the credit it earns later settles in the same one;
    /// a model the plan table lacks falls the unit back to dollars along with the cost, so the two never split.
    pub covered_unit: Option<SavingsUnit>,
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
            key_generation: 0,
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
            last_guard: None,
            hold_counted: false,
            paused: false,
            in_flight: false,
            probes: 0,
            migrations: 0,
            usage: ProbeUsage::default(),
            net: Net::default(),
            grace_left: 0,
            unanswered: Vec::new(),
            covered_since_real: false,
            covered_account: None,
            covered_unit: None,
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
    pub account: Option<String>,
    pub covered: bool,
    /// The unit the covering probe's cost settled in; `None` when the account it warmed left the registry
    /// before the probe landed, in which case the credit never fires (`same_account` cannot match a gone
    /// account either).
    pub unit: Option<SavingsUnit>,
    pub model: String,
    pub real_at: i64,
    pub ttl_ms: i64,
}

/// One real request the engine is following from its observation to its completion.
pub(crate) struct Observation {
    pub id: String,
    pub attempt: u8,
    pub started: i64,
    pub recipe: Recipe,
    pub ready: bool,
    pub expected: u64,
    pub ttl_ms: i64,
    pub prior: Option<PriorCoverage>,
    /// Observed inside a gap's backlog, so this request's own terminal event may be one the gap dropped:
    /// nothing it carries may arm a recipe.
    pub poisoned: bool,
    /// The TTL the relay actually dispatched this attempt's body with, read off `AccountSelected`; `None` when
    /// the relay left the markers as sent, in which case the cap falls back to the recipe's own received TTL.
    pub dispatched_ms: Option<i64>,
}

/// Probes a stopped recipe leaves unanswered, with the label the stop gives them. Queued under the lock and
/// written to the ledger by the engine once the lock is released.
pub(crate) struct Settlement {
    pub key: SessionKey,
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub account: Option<String>,
    pub label: Wasted,
    pub costs: Vec<f64>,
}

pub(crate) struct Inner {
    pub lifecycle: Option<LifecycleSender>,
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
    /// Probe meter tokens per subscription account for the current UTC day, band-weighted at each probe's
    /// time; rolled at midnight beside `spent_today`.
    pub meter_spent_today: BTreeMap<String, f64>,
    pub spent_day: i64,
    pub settlements: Vec<Settlement>,
    /// Due probes a gate withheld this tick, ledgered as skipped.
    pub skipped: Vec<(SessionKey, String)>,
    /// Bus gaps seen by the observer; each one discarded every observation that was still pending.
    pub bus_gaps: u64,
    /// Events a gap left queued behind it, counted down as they are applied. What arrives inside that
    /// backlog cannot be trusted: the terminal event of any request among them may be one the gap dropped.
    pub gap_backlog: usize,
    pub next_incarnation: u64,
    /// The scheduling tick's own copy of the router's fits, kept from the last successful ask when the router
    /// is gone: the plan-side cap reads an account's calibrated scale from here.
    pub fits: Vec<FitRow>,
}

pub struct Engine {
    pub(crate) inner: Mutex<Inner>,
    pub(crate) parts: EngineParts,
    pub(crate) cancel: CancellationToken,
    /// Woken by every worker that finishes, which is what `wait_idle` waits on.
    pub(crate) idle: Notify,
    pub(crate) workers: TaskTracker,
    /// How many times `step` has actually asked the router, for a test to hold against one ask per due tick.
    /// Cheap enough to keep in every build; only the reader is gated.
    router_view_calls: AtomicU64,
}

impl Engine {
    pub fn new(mut parts: EngineParts) -> Arc<Engine> {
        // `Inner` holds the only sender, so `close` dropping it is the channel ending for the consumer.
        let lifecycle = parts.lifecycle.take();
        Arc::new(Engine {
            inner: Mutex::new(Inner::empty(lifecycle)),
            parts,
            cancel: CancellationToken::new(),
            idle: Notify::new(),
            workers: TaskTracker::new(),
            router_view_calls: AtomicU64::new(0),
        })
    }

    /// Compiled only under the `debug-hooks` feature, which a dependent's dev-dependency turns on, so a
    /// release build carries no way to count the scheduler's own router traffic.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn router_views(&self) -> u64 {
        self.router_view_calls
            .load(std::sync::atomic::Ordering::Relaxed)
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
        {
            let mut inner = self.lock();
            inner.expire(now, expiry);
        }
        self.flush();
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
            // The alerts post a session's end once, so one already announced by the companion gets none here;
            // the rest end now, as they would at the expiry, since nothing will report for them again.
            if !inner.sessions[&key].ended {
                inner.emit(SessionLifecycle::SessionEnded {
                    key_id: key.0.clone(),
                    session_id: key.1.clone(),
                });
            }
        }
        // The ends above are the channel's last messages: with the sender gone the consumer's `recv()` returns
        // `None` once it has drained them, which is what the alert inputs' shutdown joins on.
        inner.lifecycle = None;
        inner.sessions.clear();
        inner.pending.clear();
        inner.agents = 0;
        inner.retained = 0;
        self.cancel.cancel();
    }
}

impl Inner {
    /// A fresh engine's own state, and the fixture a unit test builds one agent onto: the one place every field
    /// is named, so a field this struct gains needs no second literal to keep in step.
    pub(crate) fn empty(lifecycle: Option<LifecycleSender>) -> Inner {
        Inner {
            lifecycle,
            sessions: BTreeMap::new(),
            pending: HashMap::new(),
            reasons: BTreeMap::new(),
            retained: 0,
            agents: 0,
            running: 0,
            closed: false,
            spent_today: BTreeMap::new(),
            meter_spent_today: BTreeMap::new(),
            spent_day: 0,
            settlements: Vec::new(),
            skipped: Vec::new(),
            bus_gaps: 0,
            gap_backlog: 0,
            next_incarnation: 0,
            fits: Vec::new(),
        }
    }

    /// A send never blocks and a closed receiver is the alerts having shut down first, which loses nothing worth
    /// a log line.
    pub(crate) fn emit(&self, event: SessionLifecycle) {
        if let Some(tx) = &self.lifecycle {
            let _ = tx.send(event);
        }
    }

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
        self.end_observation(key, agent_id, wasted);
        self.set_reason(key, agent_id, reason);
    }

    /// `discard`'s bookkeeping without writing a reason: for a terminal state that is already the agent's
    /// current reason, so ending the observation does not count that reason a second time.
    pub(crate) fn end_observation(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
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
                key_generation: agent.key_generation,
                pool_id: agent.pool_id.clone(),
                canonical_model: agent.canonical_model.clone(),
                account: agent.account.clone(),
                label,
                costs,
            };
            self.settlements.push(settlement);
        }
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

    /// Every model name a live recipe compares against the router's rows: the recipe's own, and the one its
    /// open observation took from the recipe before it.
    pub(crate) fn retained_models(&self) -> Vec<String> {
        let mut models = BTreeSet::new();
        for (key, session) in &self.sessions {
            for (agent_id, agent) in &session.agents {
                let Some(observation) = agent.current.as_ref().filter(|_| self.live(key, agent_id))
                else {
                    continue;
                };
                models.insert(agent.canonical_model.clone());
                if let Some(prior) = &observation.prior {
                    models.insert(prior.model.clone());
                }
            }
        }
        models.into_iter().collect()
    }

    /// A resolution that landed after a recipe was retained moved the router's pins and model cooldowns to the
    /// dated id; the recipe follows, so the gate finds them and the next request's credit compares like names.
    pub(crate) fn follow_resolutions(&mut self, canonical: &HashMap<String, String>) {
        for session in self.sessions.values_mut() {
            for agent in session.agents.values_mut() {
                if let Some(dated) = canonical.get(&agent.canonical_model) {
                    agent.canonical_model = dated.clone();
                }
                if let Some(prior) = agent.current.as_mut().and_then(|o| o.prior.as_mut()) {
                    if let Some(dated) = canonical.get(&prior.model) {
                        prior.model = dated.clone();
                    }
                }
            }
        }
    }

    /// The cheap half of what a tick needs to decide before it asks the router: whether any live agent still
    /// holds a recipe. An idle session, or one whose companion has sent nothing real yet, retains nothing for
    /// the gate to read, so the tick that finds none needs no fresher view than the one it already has.
    pub(crate) fn anything_retained(&self) -> bool {
        self.sessions.iter().any(|(key, session)| {
            session
                .agents
                .iter()
                .any(|(agent_id, agent)| self.live(key, agent_id) && agent.current.is_some())
        })
    }

    /// The cap is a budget per UTC day, so the day the sum was taken for is carried with it and the first
    /// writer past midnight starts it over. Every writer of `spent_today` rolls under the same lock it
    /// charges: a probe that completes after midnight would otherwise land on yesterday's sum and have its
    /// charge cleared by the next tick. True when it rolled, so the caller deletes the stored days before it
    /// once the lock is released.
    pub(crate) fn roll_day(&mut self, now: i64) -> bool {
        let day = now.div_euclid(86_400_000);
        if self.spent_day < day {
            self.spent_today.clear();
            self.meter_spent_today.clear();
            self.spent_day = day;
            return true;
        }
        false
    }

    pub(crate) fn expire(&mut self, now: i64, expiry_ms: i64) {
        let lapsed: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.attended && !s.ended && now >= s.lease)
            .map(|(k, _)| k.clone())
            .collect();
        for key in lapsed {
            // The recipes stay: a silent companion is not an ended session, and the idle limits bound what the
            // engine spends on it. The lapse is counted once per agent, as the Go engine counted it, but no
            // agent's own reason is rewritten: a recipe the scheduler keeps probing would otherwise read held
            // in every view until its next probe.
            let agents = self.sessions[&key].agents.len() as u64;
            *self.reasons.entry(Reason::LeaseExpired).or_default() += agents;
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
            // A session whose companion reported its end was announced then; the entry outlives the report
            // only so a result still in flight finds it, and the alerts resolve on one end, not two.
            let announced = self.sessions.remove(&key).is_some_and(|s| s.ended);
            if !announced {
                self.emit(SessionLifecycle::SessionEnded {
                    key_id: key.0.clone(),
                    session_id: key.1.clone(),
                });
            }
        }
    }

    /// Ends every session whose key the registry no longer holds under the generation that registered it: a
    /// deletion, or the same id created again, takes the previous holder's sessions and recipes with it rather
    /// than leaving them to the session expiry.
    pub(crate) fn drop_sessions_of_dead_keys(&mut self, snapshot: &RegistrySnapshot) {
        let dead: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                snapshot.key(&s.key_id).map(|k| k.created_at) != Some(s.key_generation)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in dead {
            let agents: Vec<String> = self.sessions[&key].agents.keys().cloned().collect();
            for agent in &agents {
                self.discard(
                    &key,
                    agent,
                    Reason::SessionEnded,
                    Some(Wasted::SessionEnded),
                );
            }
            self.agents -= agents.len();
            let announced = self.sessions.remove(&key).is_some_and(|s| s.ended);
            if !announced {
                self.emit(SessionLifecycle::SessionEnded {
                    key_id: key.0.clone(),
                    session_id: key.1.clone(),
                });
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentStatus {
    pub account_id: Option<String>,
    pub agent_id: String,
    pub cache_ttl_ms: i64,
    pub canonical_model: String,
    pub cold: u32,
    pub failures: u32,
    pub grace_left: u32,
    pub idle_ticks: u64,
    pub in_flight: bool,
    pub last_activity: i64,
    pub migrations: u64,
    pub net_meter_tokens: f64,
    pub net_usd: f64,
    pub next_probe_at: Option<i64>,
    pub paused: bool,
    pub probe_usage: ProbeUsage,
    pub probes: u64,
    pub reason: Reason,
    pub request_generation: u64,
    pub state: AgentState,
    pub unanswered: usize,
}

/// The engine's bus task. Every event is handed to `observe` under the lock; nothing here awaits while holding
/// it, so a slow registry read never stalls the queue. The bus drops for a subscriber that falls behind and
/// counts it; a rise in that counter means events this task never saw, so every observation still open is
/// discarded before the next event is applied.
pub fn spawn_observer(engine: Arc<Engine>) -> tokio::task::JoinHandle<()> {
    let subscriber = engine.parts.bus.subscribe("keepalive");
    spawn_subscriber(subscriber, move |mut subscriber| async move {
        let mut seen_dropped = 0u64;
        while let Some(event) = subscriber.events.recv().await {
            let dropped = subscriber
                .stats
                .dropped
                .load(std::sync::atomic::Ordering::Relaxed);
            if dropped > seen_dropped {
                // What is already queued behind the gap is the backlog the engine has to distrust, so its
                // depth is read here, where the receiver is.
                engine.bus_gap(dropped - seen_dropped, subscriber.events.len());
                seen_dropped = dropped;
            }
            engine.observe(event);
        }
    })
}

impl Engine {
    pub fn observe(&self, event: BusEvent) {
        // Each arm takes the lock and gives it back before returning. The drain belongs here rather than in
        // the arms: an observation sweeps the lapsed sessions, and the settlements that sweep leaves have to
        // reach the ledger stamped with this event rather than with whatever call drains next.
        match event {
            BusEvent::RequestObserved(observed) => self.on_observed(observed),
            BusEvent::AccountSelected(selected) => self.on_selected(selected),
            BusEvent::ResponseHead(head) => self.on_head(head),
            BusEvent::ResponseChunk(chunk) => self.on_chunk(chunk),
            BusEvent::RequestComplete(complete) => self.on_complete(complete),
        }
        // One event of the backlog applied. The count is read before the arm above rather than after, so the
        // event a gap was noticed on is itself inside its own backlog.
        {
            let mut inner = self.lock();
            inner.gap_backlog = inner.gap_backlog.saturating_sub(1);
        }
        self.flush();
    }

    /// Events were lost between the last one seen and this one. Whatever was still open cannot be completed
    /// with confidence: the lost events may have been the completion, or a failover onto another account. An
    /// armed recipe's evidence was complete before the gap and stays.
    pub fn bus_gap(&self, dropped: u64, backlog: usize) {
        let mut inner = self.lock();
        inner.bus_gaps += 1;
        inner.gap_backlog = backlog;
        tracing::warn!(
            dropped,
            "the keepalive subscriber fell behind the bus; pending observations discarded"
        );
        let open: Vec<(SessionKey, String)> = inner.pending.values().cloned().collect();
        for (key, agent_id) in open {
            inner.discard(&key, &agent_id, Reason::BusGap, None);
        }
    }

    pub fn parts_bus(&self) -> Bus {
        self.parts.bus.clone()
    }

    pub fn agent_status(
        &self,
        key_id: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Option<AgentStatus> {
        let inner = self.lock();
        let agent = inner
            .sessions
            .get(&(key_id.to_string(), session_id.to_string()))?
            .agents
            .get(agent_id)?;
        Some(AgentStatus {
            account_id: agent.account.clone(),
            agent_id: agent.id.clone(),
            cache_ttl_ms: agent.ttl_ms,
            canonical_model: agent.canonical_model.clone(),
            cold: agent.cold,
            failures: agent.failures,
            grace_left: agent.grace_left,
            idle_ticks: agent.idle_ticks,
            in_flight: agent.in_flight,
            last_activity: agent.last_activity,
            migrations: agent.migrations,
            net_meter_tokens: agent.net.meter_tokens,
            net_usd: agent.net.usd,
            next_probe_at: agent.due_at(),
            paused: agent.paused,
            probe_usage: agent.usage,
            probes: agent.probes,
            reason: agent.reason,
            request_generation: agent.generation,
            state: agent.state,
            unanswered: agent.unanswered.len(),
        })
    }

    fn on_observed(&self, observed: RequestObserved) {
        // Only an inference request says anything about the cache entry a recipe is for. Token counting
        // carries the same session and agent headers and writes no cache, so it must reach nothing below:
        // not the recipe, not the schedule, and not the request time the kept-warm gap is measured from.
        if observed.origin == phoebis_bus::Origin::Keepalive
            || observed.kind != phoebis_bus::Kind::Inference
        {
            return;
        }
        let (Some(key_id), Some(session_id), Some(agent_id)) = (
            observed.key_id.clone(),
            observed.session_id.clone(),
            observed.agent_id.clone(),
        ) else {
            return;
        };
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        let limits = self.parts.limits.clone();
        let snapshot = self.parts.router.snapshot();
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        inner.expire(now, expiry);
        let key: SessionKey = (key_id, session_id);
        let Some(session) = inner.sessions.get(&key) else {
            return;
        };
        // The purge ends a session whose key id now belongs to a later holder, but only at the next tick: until
        // then this is the previous holder's session, and a request of another generation arms nothing on it.
        if session.key_generation != observed.key_generation {
            return;
        }
        if session.ended {
            return;
        }
        let Some(agent) = session.agents.get(&agent_id) else {
            return;
        };
        if agent.state == AgentState::Released {
            return;
        }
        if agent
            .current
            .as_ref()
            .is_some_and(|o| o.id == observed.request_id)
        {
            return;
        }
        // The header is an optional hardening on top of the registration rule: when both sides name an owner,
        // they must agree. Foreign traffic proves nothing about this agent, so its status, its recipe, and
        // its schedule are left as they were; only the mismatch itself is counted.
        if let (Some(registered), Some(sent)) = (&session.owner_id, &observed.owner_id) {
            if registered != sent {
                *inner.reasons.entry(Reason::OwnerMismatch).or_default() += 1;
                return;
            }
        }
        let policy = match snapshot.key(&key.0) {
            Some(row) => snapshot
                .pool(&row.pool_id)
                .map(|p| KeepalivePolicy::from_json(&p.keepalive_json))
                .unwrap_or_default(),
            None => KeepalivePolicy::default(),
        };
        // The hijack guard: an auxiliary or foreign request must not touch the recipe, the schedule, or the
        // covered-since-real flag a re-arming request's credit depends on. `reason` stays the recipe's gating
        // state, since a hold's `began` check, the completion's gate-reason match, and the view's armed-or-held
        // state all read that field as state; the agent's own `last_guard` carries the verdict itself, every
        // request's, for the view to show.
        let recipe_hashes = agent.current.as_ref().map(|observation| {
            (
                observation.recipe.static_hash.as_deref().unwrap_or(""),
                observation.recipe.tail_hash.as_deref().unwrap_or(""),
            )
        });
        let model_matches = agent
            .current
            .as_ref()
            .is_some_and(|observation| observation.recipe.model == observed.model);
        let verdict = guard(
            observed.client.request_class.as_deref(),
            observed.static_hash.as_deref(),
            &observed.boundaries,
            recipe_hashes,
            model_matches,
        );
        inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap()
            .last_guard = Some(verdict);
        if matches!(verdict, Guard::Auxiliary | Guard::Foreign) {
            return;
        }
        if agent_id != MAIN_AGENT && !policy.warm_subagents {
            // The session's last real time still moves: a subagent working is a session in use, which the main
            // agent's idle limits read.
            inner.sessions.get_mut(&key).unwrap().last_real_at = Some(now);
            inner.discard(&key, &agent_id, Reason::SubagentsOff, None);
            return;
        }
        // What the previous recipe proved, taken before anything below overwrites it and consumed with the
        // observation: the kept-warm credit reads this and nothing on the agent.
        let prior = {
            let agent = &inner.sessions[&key].agents[&agent_id];
            agent.last_real_at.map(|real_at| PriorCoverage {
                account: agent.covered_account.clone(),
                covered: agent.covered_since_real,
                unit: agent.covered_unit,
                model: agent.canonical_model.clone(),
                real_at,
                ttl_ms: agent.ttl_ms,
            })
        };
        let poisoned = inner.gap_backlog > 0;
        inner.discard(&key, &agent_id, Reason::AwaitingSelection, None);
        let session = inner.sessions.get_mut(&key).unwrap();
        session.last_real_at = Some(now);
        let agent = session.agents.get_mut(&agent_id).unwrap();
        agent.covered_since_real = false;
        agent.covered_account = None;
        agent.covered_unit = None;
        if agent.state == AgentState::Unknown {
            inner.set_reason(&key, &agent_id, Reason::AgentUnknown);
            return;
        }
        let was = agent.state;
        agent.state = AgentState::Busy;
        agent.last_activity = now;
        agent.cache_started = now;
        agent.idle_ticks = 0;
        agent.cold = 0;
        agent.failures = 0;
        agent.hold_counted = false;
        agent.last_real_at = Some(now);
        agent.canonical_model = observed.canonical_model.clone();
        agent.key_generation = observed.key_generation;
        // A real request grants the grace budget again while either balance is negative: the net only rises on
        // a request that lands warm, which needs a probe first, and which unit the next one charges is not yet
        // known here (selection has not run).
        if agent.net.usd < 0.0 || agent.net.meter_tokens < 0.0 {
            agent.grace_left = agent.policy.net_negative_grace_probes;
        }
        // The engine's own transition out of parked, which no report will repeat: the companion's next busy
        // report finds the agent already working and says nothing.
        if agent_id == MAIN_AGENT && was == AgentState::Parked {
            inner.emit(SessionLifecycle::MainResumed {
                key_id: key.0.clone(),
                session_id: key.1.clone(),
            });
        }
        if inner.pending.len() >= limits.max_agents {
            inner.set_reason(&key, &agent_id, Reason::RequestLimit);
            return;
        }
        let mut recipe = match make_recipe(
            &observed.headers,
            &observed.body,
            &observed.model,
            limits.max_request_bytes,
        ) {
            Ok(recipe) => recipe,
            Err(reason) => {
                inner.set_reason(&key, &agent_id, recipe_reason(reason));
                return;
            }
        };
        // What the next observation's guard compares against: this request's own two hashes, not the ones a
        // prior arm happened to leave behind.
        recipe.static_hash = observed.static_hash.clone();
        recipe.tail_hash = observed.boundaries.last().cloned();
        if inner.retained + recipe.size > limits.max_retained_bytes {
            inner.set_reason(&key, &agent_id, Reason::StorageLimit);
            return;
        }
        inner.retained += recipe.size;
        inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap()
            .current = Some(Observation {
            id: observed.request_id.clone(),
            attempt: 0,
            started: now,
            recipe,
            ready: false,
            expected: 0,
            ttl_ms: 0,
            prior,
            poisoned,
            dispatched_ms: None,
        });
        inner.pending.insert(observed.request_id, (key, agent_id));
    }

    fn on_selected(&self, selected: AccountSelected) {
        let snapshot = self.parts.router.snapshot();
        let verified = self.parts.limits.oauth_prewarm_verified;
        let mut inner = self.lock();
        let Some((key, agent_id)) = inner.pending.get(&selected.request_id).cloned() else {
            return;
        };
        if !inner.live(&key, &agent_id) {
            return;
        }
        let agent = inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap();
        let Some(observation) = agent.current.as_mut() else {
            return;
        };
        observation.attempt = selected.attempt;
        observation.ready = false;
        observation.expected = 0;
        observation.ttl_ms = 0;
        observation.dispatched_ms = selected.cache_ttl_ms;
        agent.cache_started = observation.started;
        agent.expires = None;
        inner.set_reason(&key, &agent_id, Reason::AwaitingSelection);
        let (policy, opted_out) = match snapshot.key(&key.0) {
            Some(row) => (
                snapshot
                    .pool(&row.pool_id)
                    .map(|p| KeepalivePolicy::from_json(&p.keepalive_json))
                    .unwrap_or_default(),
                row.keepalive_opt_out,
            ),
            None => (KeepalivePolicy::default(), true),
        };
        let pool_id = snapshot
            .key(&key.0)
            .map(|k| k.pool_id.clone())
            .unwrap_or_default();
        let refusal = if !policy.enabled {
            Some(Reason::KeepaliveOff)
        } else if opted_out {
            Some(Reason::OptedOut)
        } else {
            match snapshot.account(&selected.account_id).map(|a| a.kind) {
                Some(AccountKind::ApiKey) => None,
                Some(AccountKind::Oauth) if verified => None,
                Some(AccountKind::Oauth) => Some(Reason::OauthUnverified),
                None => Some(Reason::IneligibleCredential),
            }
        };
        if let Some(reason) = refusal {
            inner.set_reason(&key, &agent_id, reason);
            return;
        }
        let agent = inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap();
        if let Some(observation) = agent.current.as_mut() {
            // A poisoned observation never becomes ready, so no evidence of this request can arm a schedule.
            observation.ready = !observation.poisoned;
        }
        if agent.account.as_deref() != Some(selected.account_id.as_str()) {
            agent.cold = 0;
            agent.ttl_ms = 0;
        }
        agent.account = Some(selected.account_id);
        agent.pool_id = pool_id;
        agent.policy = policy;
        inner.set_reason(&key, &agent_id, Reason::AwaitingCacheEvidence);
    }

    fn on_head(&self, head: ResponseHead) {
        let mut inner = self.lock();
        let Some((key, agent_id)) = inner.pending.get(&head.request_id).cloned() else {
            return;
        };
        if !inner.live(&key, &agent_id) {
            return;
        }
        let agent = inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap();
        if let Some(observation) = agent.current.as_mut() {
            // A failed attempt is not evidence about the cache; the next selection re-arms the attempt.
            if observation.attempt == head.attempt && !(200..300).contains(&head.status) {
                observation.ready = false;
            }
        }
    }

    fn on_chunk(&self, chunk: ResponseChunk) {
        let usage = match &chunk.phase {
            Some(Phase::MessageStart { usage }) | Some(Phase::MessageDelta { usage }) => {
                ProbeUsage::from_bus(usage)
            }
            _ => return,
        };
        let now = self.now();
        let mut inner = self.lock();
        let Some((key, agent_id)) = inner.pending.get(&chunk.request_id).cloned() else {
            return;
        };
        if !inner.live(&key, &agent_id) {
            return;
        }
        let current_attempt = inner.sessions[&key].agents[&agent_id]
            .current
            .as_ref()
            .map(|o| o.attempt);
        if current_attempt != Some(chunk.attempt) {
            return;
        }
        inner.observe_usage(&key, &agent_id, usage, now);
    }

    fn on_complete(&self, complete: RequestComplete) {
        let now = self.now();
        let mut inner = self.lock();
        let Some((key, agent_id)) = inner.pending.get(&complete.request_id).cloned() else {
            return;
        };
        let reason = match complete.outcome {
            Outcome::Succeeded => None,
            Outcome::Canceled => Some(Reason::RequestCanceled),
            Outcome::Failed | Outcome::Exhausted => Some(Reason::RequestFailed),
            Outcome::Rejected => Some(Reason::RequestRejected),
        };
        if let Some(reason) = reason {
            inner.discard(&key, &agent_id, reason, None);
            return;
        }
        inner.pending.remove(&complete.request_id);
        let mut credited = None;
        // Read from the observation's own snapshot of what the previous recipe had proved: a probe that ran
        // while this request was open, or a TTL this request's evidence shortens below, cannot reach into it.
        let credit = {
            let agent = &inner.sessions[&key].agents[&agent_id];
            let read = complete
                .usage
                .as_ref()
                .and_then(|u| u.cache_read_tokens)
                .unwrap_or(0);
            agent
                .current
                .as_ref()
                .and_then(|o| o.prior.as_ref())
                .and_then(|prior| {
                    let ttl = prior.ttl_ms.max(crate::config::FIVE_MINUTES_MS);
                    let gap_exceeded = agent.last_real_at.unwrap_or(now) - prior.real_at > ttl;
                    let same_model = prior.model == agent.canonical_model;
                    // The account that served this request, after any failover, has to be the one the
                    // covering probe warmed; a prefix cached elsewhere by ordinary traffic is nobody's
                    // saving to claim.
                    let same_account = prior.account == agent.account;
                    // The probe that covered this recipe may have served on an account the registry has
                    // since dropped, which earns no credit: `same_account` would already exclude it once a
                    // real selection lands somewhere else, but a gone account funds nothing to check.
                    let unit = prior.unit;
                    (prior.covered
                        && gap_exceeded
                        && same_model
                        && same_account
                        && read > 0
                        && unit.is_some())
                    .then(|| {
                        (
                            read,
                            agent.key_generation,
                            agent.pool_id.clone(),
                            agent.account.clone(),
                            agent.canonical_model.clone(),
                            unit.unwrap(),
                        )
                    })
                })
        };
        if let Some((read, key_generation, pool_id, account, model, unit)) = credit {
            // The mark says a probe kept this request warm, which is true whatever the price table knows;
            // the saving is what the table can put a number on, zero for a model it does not price. The
            // agent's own net settles in the covering probe's unit; the ledger's kept-warm column stays
            // dollars whatever that unit was.
            let plan = &self.parts.plan;
            let table = self.parts.pricing.current();
            let saving_net = gross_saving(unit, &table, plan, &model, read).unwrap_or(0.0);
            let saving_ledger = if unit == SavingsUnit::Usd {
                saving_net
            } else {
                gross_saving(SavingsUnit::Usd, &table, plan, &model, read).unwrap_or(0.0)
            };
            inner
                .sessions
                .get_mut(&key)
                .unwrap()
                .agents
                .get_mut(&agent_id)
                .unwrap()
                .net
                .add(unit, saving_net);
            credited = Some((read, key_generation, pool_id, account, model, saving_ledger));
        }
        let (ready, armed) = inner.sessions[&key]
            .agents
            .get(&agent_id)
            .map(|a| {
                (
                    a.current.as_ref().is_some_and(|o| o.ready),
                    a.current.as_ref().is_some_and(|o| o.expected > 0),
                )
            })
            .unwrap_or((false, false));
        if ready && !armed {
            if let Some(usage) = complete.usage.as_ref().map(ProbeUsage::from_bus) {
                inner.observe_usage(&key, &agent_id, usage, now);
            }
        }
        let proved = inner.sessions[&key]
            .agents
            .get(&agent_id)
            .and_then(|a| a.current.as_ref())
            .is_some_and(|o| o.ready && o.expected > 0);
        if !proved {
            let poisoned = inner.sessions[&key]
                .agents
                .get(&agent_id)
                .and_then(|a| a.current.as_ref())
                .is_some_and(|o| o.poisoned);
            let current = inner.sessions[&key].agents.get(&agent_id).map(|a| a.reason);
            if poisoned {
                // The gap that poisoned it is what this request proved nothing under.
                inner.discard(&key, &agent_id, Reason::BusGap, None);
            } else {
                // The four gates are pool, key, or credential-kind properties that do not vary between
                // attempts of one request, so ending the observation must not recount a reason the request
                // already carries.
                match current {
                    Some(
                        Reason::KeepaliveOff
                        | Reason::OptedOut
                        | Reason::OauthUnverified
                        | Reason::IneligibleCredential,
                    ) if !ready => {
                        inner.end_observation(&key, &agent_id, None);
                    }
                    _ => inner.discard(&key, &agent_id, Reason::NoCacheEvidence, None),
                }
            }
        }
        drop(inner);
        if let Some((read, key_generation, pool_id, account, model, saving)) = credited {
            self.record_kept_warm(
                now,
                &key,
                key_generation,
                &pool_id,
                account.as_deref(),
                &model,
                &complete.request_id,
                read,
                saving,
            );
            self.parts.metrics.observe_saving(&pool_id, saving);
        }
    }
}

fn recipe_reason(reason: &str) -> Reason {
    match reason {
        "fixed_thinking" => Reason::FixedThinking,
        "forced_tool" => Reason::ForcedTool,
        "headers_too_large" => Reason::HeadersTooLarge,
        "invalid_cache_control" => Reason::InvalidCacheControl,
        "invalid_model" => Reason::InvalidModel,
        "request_too_large" => Reason::RequestTooLarge,
        "structured_output" => Reason::StructuredOutput,
        "unsupported_shape" => Reason::UnsupportedShape,
        _ => Reason::InvalidRequest,
    }
}

impl Inner {
    /// The Go `observeLocked`: evidence from a ready attempt arms the schedule at the entry's expiry, the pool's
    /// lead and this schedule's jitter taken off by `due_at`. A retry already pending stays in force.
    pub(crate) fn observe_usage(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        usage: ProbeUsage,
        _now: i64,
    ) {
        let agent = self
            .sessions
            .get_mut(key)
            .unwrap()
            .agents
            .get_mut(agent_id)
            .unwrap();
        let Some(observation) = agent.current.as_mut() else {
            return;
        };
        if !observation.ready || agent.paused || usage.evidence() == 0 {
            return;
        }
        observation.expected = usage.evidence();
        agent.ttl_ms = usage.ttl_ms(
            agent.ttl_ms,
            observation
                .dispatched_ms
                .unwrap_or(observation.recipe.ttl_ms),
        );
        if observation.ttl_ms > 0 && observation.ttl_ms < agent.ttl_ms {
            agent.ttl_ms = observation.ttl_ms;
        }
        observation.ttl_ms = agent.ttl_ms;
        agent.expires = Some(agent.cache_started + agent.ttl_ms);
        agent.jitter_frac = rand::random::<f64>();
        self.set_reason(key, agent_id, Reason::Scheduled);
    }
}

impl Engine {
    /// One tick: what is due runs, up to the worker bound. Called by the production ticker every second and by
    /// the tests directly.
    pub async fn step(self: &Arc<Self>) {
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        let max_workers = self.parts.limits.max_workers;
        // The four asks behind `router_view` are a round trip apiece; a tick with nothing due pays none of
        // them. `drop_sessions_of_dead_keys` reads the snapshot alone, which `RouterHandle::snapshot` answers
        // without a command.
        let (retained_models, rolled) = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            inner.expire(now, expiry);
            inner.drop_sessions_of_dead_keys(&self.parts.router.snapshot());
            let rolled = inner.roll_day(now);
            (
                inner.anything_retained().then(|| inner.retained_models()),
                rolled,
            )
        };
        if rolled {
            self.purge_budgets_before(now);
        }
        let Some(retained_models) = retained_models else {
            return;
        };
        let view = self.router_view(now, retained_models).await;
        let jobs = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            inner.follow_resolutions(&view.canonical);
            let mut due: Vec<(i64, SessionKey, String)> = Vec::new();
            let candidates: Vec<(SessionKey, String)> = inner
                .sessions
                .iter()
                .flat_map(|(key, s)| s.agents.keys().map(move |a| (key.clone(), a.clone())))
                .collect();
            for (key, agent_id) in candidates {
                if !inner.live(&key, &agent_id) {
                    continue;
                }
                let agent = &inner.sessions[&key].agents[&agent_id];
                // The idle limit and the gate run for every retained recipe, armed or not: a pause clears the
                // schedule, so an opt-out could otherwise never discard a recipe paused by a 429 and the idle
                // limit could never settle it while the companion keeps reporting.
                if agent.current.is_none() {
                    continue;
                }
                let ready = agent
                    .current
                    .as_ref()
                    .is_some_and(|o| o.ready && o.expected > 0);
                if inner.idle_limited(&key, &agent_id, now) {
                    inner.discard(
                        &key,
                        &agent_id,
                        Reason::IdleLimit,
                        wasted_label(Reason::IdleLimit),
                    );
                    continue;
                }
                let held = {
                    // One reborrow, so the gate reads the day's spend per pool while it holds the agent:
                    // the pool it resolves is the one whose budget bounds this probe.
                    let inner = &mut *inner;
                    let spent = &inner.spent_today;
                    let meter_spent = &inner.meter_spent_today;
                    let agent = inner
                        .sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(&agent_id)
                        .unwrap();
                    gate(
                        &view,
                        agent,
                        &key.1,
                        spent,
                        meter_spent,
                        &self.parts.windows,
                        &self.parts.plan,
                    )
                };
                match held {
                    Some(Gate::Drop(reason, label)) => {
                        inner.discard(&key, &agent_id, reason, Some(label));
                        continue;
                    }
                    Some(Gate::Hold(reason)) => {
                        // One hold withholds one probe, whatever number of ticks it spans: the production
                        // ticker runs each second, so a five-hour cooldown would otherwise ledger 18,000
                        // withheld probes for the one it actually withheld. A hold that begins before the
                        // due time has withheld nothing yet, so the skip waits for the due time to pass.
                        // The due time is read after the gate, so the policy the gate just refreshed
                        // decides whether a probe was due, and a probe already in flight was not withheld.
                        let agent = &inner.sessions[&key].agents[&agent_id];
                        let began = agent.reason != reason;
                        let counted = agent.hold_counted && !began;
                        let was_due =
                            !agent.in_flight && agent.due_at().is_some_and(|at| at <= now);
                        if !counted && was_due {
                            inner.skipped.push((key.clone(), agent_id.clone()));
                        }
                        if let Some(agent) = inner
                            .sessions
                            .get_mut(&key)
                            .and_then(|s| s.agents.get_mut(&agent_id))
                        {
                            agent.hold_counted = counted || was_due;
                        }
                        if began {
                            inner.set_reason(&key, &agent_id, reason);
                        }
                        continue;
                    }
                    None => {}
                }
                if !ready || inner.sessions[&key].agents[&agent_id].in_flight {
                    continue;
                }
                // The gate may have refreshed the agent's policy from the pool, so the due time here reads
                // whatever tuning edit just landed rather than the one this tick started with.
                let Some(at) = inner.sessions[&key].agents[&agent_id].due_at() else {
                    continue;
                };
                if at <= now {
                    due.push((at, key, agent_id));
                }
            }
            due.sort();
            let mut jobs = Vec::new();
            for (_, key, agent_id) in due {
                if inner.running >= max_workers {
                    break;
                }
                let idle = agent_id == MAIN_AGENT
                    && inner.sessions[&key].agents[&agent_id].state == AgentState::Idle
                    || !inner.sessions[&key].attended;
                let agent = inner
                    .sessions
                    .get_mut(&key)
                    .unwrap()
                    .agents
                    .get_mut(&agent_id)
                    .unwrap();
                let observation = agent.current.as_ref().unwrap();
                let job = ProbeJob {
                    key: key.clone(),
                    key_generation: agent.key_generation,
                    agent_id: agent_id.clone(),
                    incarnation: agent.incarnation,
                    observation_id: observation.id.clone(),
                    generation: agent.generation,
                    attempt: observation.attempt,
                    started: now,
                    account: agent.account.clone(),
                    pool_id: agent.pool_id.clone(),
                    canonical_model: agent.canonical_model.clone(),
                    recipe: observation.recipe.clone(),
                    expected: observation.expected,
                };
                agent.in_flight = true;
                agent.hold_counted = false;
                agent.probes += 1;
                if idle {
                    agent.idle_ticks += 1;
                }
                inner.running += 1;
                inner.set_reason(&key, &agent_id, Reason::Probing);
                // The tracker counts the job from here, under the lock that reserved the worker: the spawn
                // below happens after the lock is released, and a `shutdown` that passes between the two
                // would otherwise find an empty tracker and return while this job was still to come.
                jobs.push((job, self.workers.token()));
            }
            jobs
        };
        self.flush();
        for (job, token) in jobs {
            let engine = self.clone();
            self.workers.spawn(async move {
                // Bound rather than dropped here: the tracker counts the job while the token lives.
                let _token = token;
                run_job(engine, job).await;
            });
        }
    }

    /// Resolves once no probe is in flight; the tests drive `step` and then wait here.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            // Registered before the counter is read, so a worker that finishes between the two wakes this
            // waiter instead of notifying nobody.
            notified.as_mut().enable();
            if self.lock().running == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Stops the scheduler, cancels the probes in flight, and waits for their tasks: the caller drains the
    /// listeners after this, so a probe never outlives the relay it runs through.
    pub async fn shutdown(&self) {
        self.close();
        self.workers.close();
        self.workers.wait().await;
    }
}

pub(crate) struct RouterView {
    /// A retained model name to the one the router keys its rows on, for the names that differ.
    pub canonical: HashMap<String, String>,
    pub cooled: BTreeSet<(String, String)>,
    /// The scheduling tick's own copy of the router's fits, refreshed here and kept from the last successful
    /// ask when the router is gone.
    pub fits: Vec<FitRow>,
    pub overage: BTreeSet<String>,
    /// `(key, session, canonical model)` to the pinned account: the account a probe would actually run on.
    pub pins: BTreeMap<(String, String, String), String>,
    pub snapshot: Arc<RegistrySnapshot>,
    /// A subscription account's own `organization.rate_limit_tier`, for the plan-side cap's fallback when the
    /// router has not fitted the account's window yet.
    pub tiers: HashMap<String, String>,
}

/// What the gate decided for a due recipe: wait with a reason, or drop it with the label its probes settle under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Drop(Reason, Wasted),
    Hold(Reason),
}

impl Engine {
    /// Four asks of the router task per tick, whatever the number of recipes: the scheduler must not queue one
    /// command per agent in front of selection.
    pub(crate) async fn router_view(&self, now: i64, retained_models: Vec<String>) -> RouterView {
        self.router_view_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let snapshot = self.parts.router.snapshot();
        let canonical = self
            .parts
            .router
            .canonicals(retained_models.clone())
            .await
            .map(|answered| {
                retained_models
                    .into_iter()
                    .zip(answered)
                    .filter(|(retained, canonical)| retained != canonical)
                    .collect()
            })
            .unwrap_or_default();
        let pins = self
            .parts
            .router
            .pins()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| ((p.key_id, p.session_id, p.model), p.account_id))
            .collect();
        let cooled = self
            .parts
            .router
            .cooldowns()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.until > now)
            .map(|c| (c.account_id, c.scope))
            .collect();
        let fallback_ms = self.parts.cooldown_fallback.as_millis() as i64;
        let overage = self
            .parts
            .router
            .overages()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, e)| e.stands(now, fallback_ms))
            .map(|(id, _)| id)
            .collect();
        // On `RouterGone` the copy already held stands: the plan-side cap reads a stale fit sooner than none.
        let fits = match self.parts.router.fits().await {
            Ok(fits) => {
                self.lock().fits = fits.clone();
                fits
            }
            Err(_) => self.lock().fits.clone(),
        };
        // Read beside the fits copy, for the same cap: an account the router has not calibrated yet still has
        // a tier the profile named, which bounds the cap until a fit arrives.
        let tiers = snapshot
            .accounts()
            .filter(|a| a.kind == AccountKind::Oauth)
            .filter_map(|a| {
                let tier = self.parts.store.subscription(&a.id).ok().flatten()?.tier?;
                Some((a.id.clone(), tier))
            })
            .collect();
        RouterView {
            canonical,
            cooled,
            fits,
            overage,
            pins,
            snapshot,
            tiers,
        }
    }

    /// The recipes one session holds, for a test or a view that asks by ids.
    pub fn recipes_for(&self, key_id: &str, session_id: &str) -> Vec<String> {
        let inner = self.lock();
        inner
            .sessions
            .get(&(key_id.to_string(), session_id.to_string()))
            .map(|s| {
                s.agents
                    .values()
                    .filter(|a| a.current.is_some())
                    .map(|a| a.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn spawn_scheduler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => engine.step().await,
                    _ = engine.cancel.cancelled() => return,
                }
            }
        })
    }
}

/// The spec's "Scheduling against router state" row, in its order. `Some` names why the due probe waits, or
/// why the recipe goes.
pub(crate) fn gate(
    view: &RouterView,
    agent: &mut Agent,
    session_id: &str,
    spent_today: &BTreeMap<String, f64>,
    meter_spent_today: &BTreeMap<String, f64>,
    windows: &PlanWindows,
    plan: &PlanFactors,
) -> Option<Gate> {
    let key_id = agent.key_id.clone();
    // The operator's opt-out reaches an idle session here, not only at its next request: the recipe is dropped
    // rather than held, because nothing on that key may be probed again until the flag is cleared.
    let Some(key) = view.snapshot.key(&key_id).filter(|k| !k.keepalive_opt_out) else {
        return Some(Gate::Drop(Reason::KeyOptedOut, Wasted::Released));
    };
    let Some(pool) = view.snapshot.pool(&key.pool_id).filter(|p| p.enabled) else {
        return Some(Gate::Hold(Reason::PoolDisabled));
    };
    if pool.affinity_ttl_seconds < MIN_AFFINITY_SECONDS {
        return Some(Gate::Hold(Reason::AffinityTooShort));
    }
    let policy = KeepalivePolicy::from_json(&pool.keepalive_json);
    // The pool's tuning may have changed since admission, and an admin may have moved the key to another
    // pool without a request following; the due time, the budget, and the probe's own delta all read the pool
    // the key belongs to now.
    agent.policy = policy.clone();
    agent.pool_id = key.pool_id.clone();
    if !policy.enabled {
        return Some(Gate::Hold(Reason::KeepaliveOff));
    }
    // The operator's switch reaches a recipe armed before it was flipped, the way the key opt-out does: dropped
    // rather than held, since nothing about a subagent may be probed again until the option comes back.
    if agent.id != MAIN_AGENT && !policy.warm_subagents {
        return Some(Gate::Drop(Reason::SubagentsOff, Wasted::Released));
    }
    // The pin is what the probe will follow, so the pin's account is the one to gate: a sibling agent's real
    // request may have moved it since this recipe last ran.
    let pinned = view.pins.get(&(
        key_id.clone(),
        session_id.to_string(),
        agent.canonical_model.clone(),
    ));
    if let Some(pinned) = pinned {
        agent.account = Some(pinned.clone());
    }
    let Some(account) = agent
        .account
        .as_deref()
        .and_then(|id| view.snapshot.account(id))
    else {
        return Some(Gate::Hold(Reason::Paused));
    };
    if !account.enabled {
        return Some(Gate::Hold(Reason::Paused));
    }
    if matches!(
        account.state,
        AccountState::ReauthRequired | AccountState::FundingRequired
    ) {
        return Some(Gate::Hold(Reason::Reauth));
    }
    let model_scope = format!("model:{}", agent.canonical_model);
    if view
        .cooled
        .contains(&(account.id.clone(), "account".into()))
        || view.cooled.contains(&(account.id.clone(), model_scope))
    {
        return Some(Gate::Hold(Reason::Cooled));
    }
    if view.overage.contains(&account.id) && pool.overage_policy != OveragePolicy::Allow {
        return Some(Gate::Hold(Reason::Overage));
    }
    // The cap bounds the pool the probe runs under, which the refresh above may have just moved.
    let spent = spent_today.get(&agent.pool_id).copied().unwrap_or(0.0);
    if policy
        .probe_cost_cap_usd_per_day
        .is_some_and(|cap| spent >= cap)
    {
        return Some(Gate::Hold(Reason::CapReached));
    }
    let in_overage = view.overage.contains(&account.id);
    // The plan side counts a subscription account's probe meter tokens against the share of its 5-hour window,
    // scaled by the fitted row when the router has calibrated one, else by the tier's predicted window; skipped
    // only when the account has neither.
    if let Some(share) = policy.probe_cap_window_share_per_day {
        if account.kind == AccountKind::Oauth {
            let spent_meter = meter_spent_today.get(&account.id).copied().unwrap_or(0.0);
            let scale = view
                .fits
                .iter()
                .find(|row| row.account_id == account.id && row.window == FIVE_HOUR)
                .map(|row| row.scale)
                .filter(|scale| scale.is_finite() && *scale > 0.0)
                // No fit yet: the tier's predicted window bounds the cap until the router calibrates one.
                .or_else(|| {
                    view.tiers
                        .get(&account.id)
                        .and_then(|tier| windows.predicted(tier, FIVE_HOUR))
                });
            if let Some(scale) = scale {
                if spent_meter / scale >= share {
                    return Some(Gate::Hold(Reason::CapReached));
                }
            }
        }
    }
    let default_unit = Funding::of(account.kind, in_overage).unit();
    // A model the plan table lacks settles every probe in dollars whatever the funding, the same fall-back
    // `settled_cost` takes; the stop has to read the balance in that unit too, or a zero meter balance masks a
    // negative dollar one, which the meter cap never catches and the dollar cap catches only when set.
    let unit = if default_unit == SavingsUnit::MeterTokens
        && plan.factor(&agent.canonical_model).is_none()
    {
        SavingsUnit::Usd
    } else {
        default_unit
    };
    if policy.stop_when_net_negative && agent.net.in_unit(unit) < 0.0 && agent.grace_left == 0 {
        return Some(Gate::Hold(Reason::NetNegative));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refreshing_class_rearms_whatever_the_chain_says() {
        for class in phoebis_routing::REFRESHING_CLASSES.iter().copied() {
            assert_eq!(
                guard(Some(class), None, &[], Some(("s", "t")), false),
                Guard::Rearm,
                "{class}"
            );
        }
    }

    #[test]
    fn any_other_class_is_auxiliary() {
        assert_eq!(
            guard(Some("title"), None, &[], Some(("s", "t")), false),
            Guard::Auxiliary
        );
    }

    #[test]
    fn a_first_observation_with_no_recipe_rearms_whatever_it_says() {
        assert_eq!(
            guard(None, Some("s"), &["t".to_string()], None, false),
            Guard::Rearm
        );
    }

    #[test]
    fn a_body_without_a_chain_rearms_every_request() {
        assert_eq!(
            guard(None, None, &[], Some(("s", "t")), false),
            Guard::Rearm
        );
    }

    #[test]
    fn boundaries_holding_the_recipes_tail_rearm() {
        assert_eq!(
            guard(
                None,
                Some("other"),
                &["a".into(), "t".into()],
                Some(("s", "t")),
                false
            ),
            Guard::Rearm
        );
    }

    #[test]
    fn the_same_static_prefix_and_model_without_a_continuation_restarts() {
        assert_eq!(
            guard(None, Some("s"), &["fresh".into()], Some(("s", "t")), true),
            Guard::Restart
        );
    }

    #[test]
    fn the_same_static_prefix_on_another_model_is_foreign_not_a_restart() {
        assert_eq!(
            guard(None, Some("s"), &["fresh".into()], Some(("s", "t")), false),
            Guard::Foreign
        );
    }

    #[test]
    fn neither_the_chain_nor_the_static_prefix_matching_is_foreign() {
        assert_eq!(
            guard(
                None,
                Some("other"),
                &["fresh".into()],
                Some(("s", "t")),
                false
            ),
            Guard::Foreign
        );
    }
}
