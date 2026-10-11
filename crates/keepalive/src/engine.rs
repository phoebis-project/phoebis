//! The engine: every session the companion registered or a request revealed, every agent's recipe, and the
//! counters behind the views. One mutex, as the Go engine had, held for microseconds at a time; nothing under
//! it does I/O.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{
    spawn_subscriber, AccountSelected, Bus, Delivery, Event as BusEvent, Outcome, Phase,
    RequestComplete, RequestObserved, ResponseChunk, ResponseHead,
};
use phoebis_ledger::{Ledger, Metrics, PlanTables, Pricing};
use phoebis_oauth::Clock;
use phoebis_relay::{CacheTtlPolicy, Relay};
use phoebis_routing::{budget_deadline, listing, MemberSpend, RegistrySnapshot, RouterHandle};
use phoebis_store::{AccountKind, AccountRow, AccountState, OveragePolicy, PoolRow, Store};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::breakeven::{
    resolve, resolve_pool, shipped_tightest, Ceilings, FactorRows, ResolvedCeilings,
};
use crate::config::{
    KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, FIVE_MINUTES_MS, MAX_WORKERS,
};
use crate::persist::Dirty;
use crate::probe::{ceiling, run_job, wasted_label, ProbeJob};
use crate::recipe::{make_recipe, Recipe};
use crate::usage::ProbeUsage;
use crate::view::{recipe_state, RecipeState};

const LOCK_POISONED: &str = "keepalive engine poisoned by a prior panic";

/// The main conversation's agent, which the idle limits apply to.
pub const MAIN_AGENT: &str = "main";

pub type SessionKey = (String, String);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
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

/// What a parked agent waits on: a background task of its own, a scheduled wakeup, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkKind {
    Background,
    Both,
    Scheduled,
}

impl ParkKind {
    pub fn parse(s: &str) -> Option<ParkKind> {
        Some(match s {
            "background" => ParkKind::Background,
            "both" => ParkKind::Both,
            "scheduled" => ParkKind::Scheduled,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ParkKind::Background => "background",
            ParkKind::Both => "both",
            ParkKind::Scheduled => "scheduled",
        }
    }
}

/// Why probes a recipe paid for went unanswered: the stop that left them, or the request after them that read no
/// cache. The label the wasted count carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wasted {
    ColdReturn,
    Evicted,
    Expired,
    Horizon,
    IdleLimit,
    ParkedLimit,
    Released,
    Replaced,
    SessionEnded,
}

impl Wasted {
    /// Every label, so a reader lists the ones no probe carried at zero rather than leaving them out.
    pub const ALL: [Wasted; 9] = [
        Wasted::ColdReturn,
        Wasted::Evicted,
        Wasted::Expired,
        Wasted::Horizon,
        Wasted::IdleLimit,
        Wasted::ParkedLimit,
        Wasted::Released,
        Wasted::Replaced,
        Wasted::SessionEnded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Wasted::ColdReturn => "cold_return",
            Wasted::Evicted => "evicted",
            Wasted::Expired => "expired",
            Wasted::Horizon => "horizon",
            Wasted::IdleLimit => "idle_limit",
            Wasted::ParkedLimit => "parked_limit",
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
    Budget => "budget",
    BusGap => "bus_gap",
    Cooled => "cooled",
    Evicted => "evicted",
    Expired => "expired",
    ExpiredAtRestart => "expired_at_restart",
    FixedThinking => "fixed_thinking",
    ForcedTool => "forced_tool",
    HeadersTooLarge => "headers_too_large",
    Horizon => "horizon",
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
    LedgerBehind => "ledger_behind",
    MissingAccount => "missing_account",
    NoCacheEvidence => "no_cache_evidence",
    NoProbeCacheEvidence => "no_probe_cache_evidence",
    OptedOut => "opted_out",
    Overage => "overage",
    OwnerMismatch => "owner_mismatch",
    ParkedLimit => "parked_limit",
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
    UnexpectedOutput => "unexpected_output",
    UnsupportedProbe => "unsupported_probe",
    UnsupportedShape => "unsupported_shape",
}

/// A cause leaves the engine as its word, the one the counters and the stored payload already key it by.
impl serde::Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

const DAY_MS: i64 = 86_400_000;

/// The UTC day `ms` falls on, counted from the epoch. The day counters and their reader both go through it, so the
/// two never disagree on where a day ends.
pub(crate) fn utc_day(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// What an observation does to the agent's recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// The lower bounds of the ten idle-age buckets, in seconds: 0 to 5, 5 to 15, 15 to 30 and 30 to 60 minutes, then
/// 1 to 2, 2 to 4, 4 to 8, 8 to 16 and 16 to 32 hours, and above.
pub const IDLE_AGE_BOUNDS_SECONDS: [u64; 10] = [
    0, 300, 900, 1_800, 3_600, 7_200, 14_400, 28_800, 57_600, 115_200,
];

/// The bucket of an idle window `age_ms` old. An age below zero, from a clock that stepped back, reads as the first.
pub fn idle_age_bucket(age_ms: i64) -> usize {
    IDLE_AGE_BOUNDS_SECONDS
        .iter()
        .rposition(|&bound| age_ms >= bound as i64 * 1_000)
        .unwrap_or(0)
}

/// One idle-age bucket of a pool: the idle windows of main that reached the bucket's start, and those whose next
/// request from main continued the same prefix inside the bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdleReturnBucket {
    pub reached: u64,
    pub returned: u64,
}

/// One pool's counters, a bucket per bound of `IDLE_AGE_BOUNDS_SECONDS`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdleReturns {
    pub buckets: [IdleReturnBucket; 10],
}

/// One open idle window of main: when it began, the highest bucket `reached` has been raised for, the prefix main's
/// last admitted request carried when the window opened (kept apart from the recipe, so a return is judged against it
/// after a ceiling discarded the recipe and on a pool that retains none), the moment a prompt announced the return,
/// when one has, and the pool the window counts under, fixed at the open so a key that moves pools mid-window keeps
/// the window's buckets in one pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdleEpisode {
    pub started: i64,
    pub bucket: Option<usize>,
    pub anchor: Option<(String, String)>,
    pub prompt_at: Option<i64>,
    pub pool_key: String,
}

/// Whether a request continues the prefix `anchor` names: one of its message boundaries equals the anchor's tail hash.
/// The chain is the only evidence, and the request's class is none: a refreshing class whose chain lacks the tail
/// hash answers false where `guard` re-arms it, as do a request with no chain and an absent anchor.
pub fn continues_prefix(anchor: Option<(&str, &str)>, boundaries: &[String]) -> bool {
    anchor.is_some_and(|(_, tail)| boundaries.iter().any(|boundary| boundary == tail))
}

/// Raises `reached` in the buckets after `counted` up to `bucket`. A tick and the request that ends a window both
/// count through here, and `counted` is the fence that keeps them from raising one bucket twice for one window. A
/// bucket at or below the fence raises nothing: the wall clock can step back, and a tick can take the lock with a
/// later `now` than a request or a report that read the clock before it.
fn raise_reached(counters: &mut IdleReturns, counted: Option<usize>, bucket: usize) {
    let from = counted.map_or(0, |c| c + 1);
    for counter in counters.buckets.iter_mut().take(bucket + 1).skip(from) {
        counter.reached += 1;
    }
}

/// The pool a window counts under, read once at its open: the pool of the session's key in the snapshot, else the
/// agent's own pool id, else the pinned account's own id for an account outside every pool, so no window counts under
/// the empty key, which no pool row reads. The key comes first because the agent's pool id is a copy only an admitted
/// selection or a retained recipe's tick refreshes.
fn idle_returns_key(snapshot: &RegistrySnapshot, agent: &Agent) -> Option<String> {
    snapshot
        .key(&agent.key_id)
        .and_then(|key| snapshot.pool(&key.pool_id))
        .map(|pool| pool.id.clone())
        .or_else(|| (!agent.pool_id.is_empty()).then(|| agent.pool_id.clone()))
        .or_else(|| agent.account.clone())
}

/// Opens main's idle window when none is counted and main is idle or parked, from a start past the floor. The report
/// that moves main there opens it, so a prompt inside the same second still ends a window, and the tick opens the
/// ones a presence, a park's end or a load left unopened. True when a window opened.
pub(crate) fn open_idle_episode(main: &mut Agent, snapshot: &RegistrySnapshot) -> bool {
    if main.idle_episode.is_some()
        || matches!(
            main.state,
            AgentState::Busy | AgentState::Released | AgentState::Unknown
        )
    {
        return false;
    }
    let start = main.idle_window_start();
    if main.idle_window_floor.is_some_and(|floor| start <= floor) {
        return false;
    }
    // One lookup per window, so a key that moves pools takes its next window with it.
    let Some(pool_key) = idle_returns_key(snapshot, main) else {
        return false;
    };
    main.idle_episode = Some(IdleEpisode {
        started: start,
        bucket: None,
        anchor: main.last_prefix.clone(),
        prompt_at: None,
        pool_key,
    });
    true
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

    /// `Plan` is the share of the account's 5-hour window, the other two dollars: `BurnUnit::of`'s rule.
    pub fn unit(self) -> SavingsUnit {
        match self {
            Funding::Plan => SavingsUnit::WindowShare,
            Funding::ApiKey | Funding::Overage => SavingsUnit::Usd,
        }
    }
}

/// The word an account kind is spelled with on the bus's `AccountSelected` and in the persisted payloads, whose
/// spelling the store keeps to itself.
pub(crate) fn kind_word(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::ApiKey => "api_key",
        AccountKind::Oauth => "oauth",
    }
}

/// `kind_word` read back, `None` for a word no kind is spelled with.
pub(crate) fn kind_of_word(word: &str) -> Option<AccountKind> {
    match word {
        "api_key" => Some(AccountKind::ApiKey),
        "oauth" => Some(AccountKind::Oauth),
        _ => None,
    }
}

pub struct EngineParts {
    pub bus: Bus,
    pub clock: Clock,
    /// The overage predicate's fallback: how long evidence naming no reset stands past its observation, the
    /// same value the relay's `Limits` gets, so the probe gate and the selector read one account the same way.
    pub cooldown_fallback: Duration,
    pub ledger: Ledger,
    pub limits: KeepaliveLimits,
    /// The relay's per-body bound: the largest body a recipe may retain, so every request the relay accepts is
    /// retainable.
    pub max_request_bytes: usize,
    pub metrics: Arc<Metrics>,
    pub pricing: Arc<Pricing>,
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    pub session_expiry: Duration,
    /// The registry's own store, read for a subscription account's tier: the window a plan-funded probe is a share
    /// of, and the sessions and recipes a restart resumes.
    pub store: Arc<Store>,
    /// The subscription cost model, for a probe on a plan-funded attempt.
    pub tables: Arc<PlanTables>,
    /// The schedule's leads, jitters and retry limits: `KeepaliveTiming::for_start()` in the binary, and a test's
    /// own numbers where it asserts exact times.
    pub timing: KeepaliveTiming,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub agents: usize,
    pub bus_gaps: u64,
    /// The request events (a selection, a head, a chunk, a completion, a new request) and the reports refused because
    /// they arrived after the shutdown's quiesce, so the state the last save holds is the state at the quiesce.
    pub late_updates: u64,
    /// The sessions `max_sessions` bounds: an ended session keeps its entry until the expiry and holds no slot.
    pub live_sessions: usize,
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
    /// A session the load restored from a save that held it attended keeps the attended reading of the ceilings until
    /// this instant or until the mod registers it again, whichever is first: the mod re-adopts a restored session
    /// within its lease, and a parked recipe must not be discarded as idle before it can.
    pub restored_until: Option<i64>,
    pub agents: BTreeMap<String, Agent>,
}

impl Session {
    /// Whether the ceilings read the session as attended at `now`: as the mod's lease says, or as the save said for
    /// one lease length after a load.
    pub(crate) fn attended_at(&self, now: i64) -> bool {
        self.attended || self.restored_until.is_some_and(|until| now < until)
    }
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
    /// Written through `set_state`, which ends the park on any move out of `Parked`.
    pub state: AgentState,
    // The park the last `parked` report described, cleared with `park_began` on every move out of `Parked`, so a
    // reader never finds a park on an agent that is not parked.
    pub deadline: Option<i64>,
    pub park_kind: Option<ParkKind>,
    pub wake_at: Option<i64>,
    /// When the current park was reported, kept across a repeated `parked` of the same kind.
    pub park_began: Option<i64>,
    /// When a deadline or a wake passed without a request: the idle window restarts here. No report clears it,
    /// since the window starts at the later of it and `last_activity` and an older end never outranks a newer
    /// request.
    pub park_ended_at: Option<i64>,
    /// The open idle window of main, `None` while none is counted.
    pub idle_episode: Option<IdleEpisode>,
    /// The start of main's last closed idle window. The next opens only from a start past it, so a close that leaves
    /// the start in place, a foreign request or a lapse a registration adopts again, never reopens the same window.
    /// A close that restarts the window on purpose (a request, presence, a park's end, a revival) clears it: the
    /// floor orders starts, and a clock that stepped back before the restart is no ordering.
    pub idle_window_floor: Option<i64>,
    /// The static and tail hash of main's last admitted request, retained or not, which the next idle window judges a
    /// return against.
    pub last_prefix: Option<(String, String)>,
    pub generation: u64,
    pub current: Option<Observation>,
    /// The recipe was set or replaced since the last landed save that carried its body, so the next save seals it.
    pub body_dirty: bool,
    /// Whether the registry may hold a sealed body under this agent's row, so a save without a recipe clears it. A new
    /// record starts true, since it can take over a row an earlier record sealed a body into, and an acknowledged seal
    /// or clear sets it to what the registry then holds.
    pub body_saved: bool,
    /// When the entry the recipe keeps warm expires. The due time is this less the timing's lead and jitter.
    pub expires: Option<i64>,
    /// The share of the timing's jitter this schedule takes, rolled once per schedule so the due time holds
    /// still between ticks.
    pub jitter_frac: f64,
    /// An outstanding backoff after a failed probe; the due time is the later of it and the expiry's.
    pub retry_at: Option<i64>,
    /// The pool's policy as of the last admission or tick, so the ceilings never need the registry.
    pub policy: KeepalivePolicy,
    /// The engine's timing, held here so a due time and a retry never need the engine.
    pub timing: KeepaliveTiming,
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
    /// Every probe since the last real request with the entry it wrote, settled as wasted when the recipe stops, and
    /// the newest of them the coverage the next request reads.
    pub unanswered: Vec<UnansweredProbe>,
    /// The probes a superseding real request took over from `unanswered`, with the keys they were paid under: the
    /// request moves the agent's model, account, pool and key generation before its completion decides them. A
    /// probe sent while it is open lands on `unanswered`, for the request after it. The idle limit moves its probes
    /// here too, pending under its label until their entry lapses or a turn reads them.
    pub kept: Vec<Kept>,
    pub last_real_at: Option<i64>,
    /// The label of the last discard that had one, so a probe result arriving after the recipe stopped settles
    /// under the stop that orphaned it.
    pub last_stop: Option<Wasted>,
    /// Main's last presence report: when the person was last seen at the terminal.
    pub last_presence_at: Option<i64>,
    pub last_probe_at: Option<i64>,
    /// The probes sent on `counters_day`, the UTC day both counts are for.
    pub probes_today: u64,
    /// The requests credited as kept warm on `counters_day`.
    pub credited_today: u64,
    pub counters_day: i64,
}

impl Agent {
    pub(crate) fn new(
        id: &str,
        key_id: &str,
        incarnation: u64,
        now: i64,
        timing: KeepaliveTiming,
    ) -> Agent {
        Agent {
            id: id.into(),
            key_id: key_id.into(),
            key_generation: 0,
            incarnation,
            state: AgentState::Busy,
            deadline: None,
            park_kind: None,
            wake_at: None,
            park_began: None,
            park_ended_at: None,
            idle_episode: None,
            idle_window_floor: None,
            last_prefix: None,
            generation: 0,
            current: None,
            body_dirty: false,
            body_saved: true,
            expires: None,
            jitter_frac: 0.0,
            retry_at: None,
            policy: KeepalivePolicy::default(),
            timing,
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
            unanswered: Vec::new(),
            kept: Vec::new(),
            last_real_at: None,
            last_stop: None,
            last_presence_at: None,
            last_probe_at: None,
            probes_today: 0,
            credited_today: 0,
            counters_day: utc_day(now),
        }
    }

    /// When the next probe is due: the entry's expiry less the timing's lead and this schedule's share of the
    /// jitter, or the outstanding backoff, whichever is later. `None` while nothing is armed.
    pub(crate) fn due_at(&self) -> Option<i64> {
        if self.paused {
            return None;
        }
        let from_expiry = self.expires.map(|expires| {
            let jitter = (self.timing.jitter_ms(self.ttl_ms) as f64 * self.jitter_frac) as i64;
            expires - self.timing.lead_ms(self.ttl_ms) - jitter
        });
        match (from_expiry, self.retry_at) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(i64::MIN).max(b.unwrap_or(i64::MIN))),
        }
    }

    /// The due time a reader is shown: none while the recipe sits under one of the two holds placed ahead of the
    /// gate, `horizon` and a disabled pool, which withhold every probe until they end or the entry lapses. The
    /// scheduler reads `due_at`, so a withheld probe is still counted once.
    pub(crate) fn next_probe_at(&self) -> Option<i64> {
        if matches!(self.reason, Reason::Horizon | Reason::PoolDisabled) {
            return None;
        }
        self.due_at()
    }

    /// Where main's idle window starts now: the later of its last activity and a park's end without a request.
    pub(crate) fn idle_window_start(&self) -> i64 {
        self.park_ended_at
            .map_or(self.last_activity, |ended| ended.max(self.last_activity))
    }

    /// Every close of an idle window: the window start in force at the close becomes the floor the next window's start
    /// has to pass. A prompt answered by a local command moves the start without closing the window, so the
    /// episode's own `started` can lie behind it. A closer that moves the start calls this first.
    pub(crate) fn close_idle_episode(&mut self) -> Option<IdleEpisode> {
        let episode = self.idle_episode.take()?;
        self.idle_window_floor = Some(episode.started.max(self.idle_window_start()));
        Some(episode)
    }

    /// The expiry stays, so the lapse still drops the prompt when the entry goes cold, as it does for a held recipe.
    pub(crate) fn pause(&mut self) {
        self.paused = true;
        self.retry_at = None;
    }

    /// Starts both day counts over when `now` falls on a later UTC day than the one they count. Only the two events
    /// they count call it, so no tick resets them: `counts_today` reads a passed day as zero instead.
    fn roll_counters_day(&mut self, now: i64) {
        let day = utc_day(now);
        if self.counters_day != day {
            self.counters_day = day;
            self.probes_today = 0;
            self.credited_today = 0;
        }
    }

    pub(crate) fn count_probe(&mut self, now: i64) {
        self.roll_counters_day(now);
        self.last_probe_at = Some(now);
        self.probes_today += 1;
    }

    pub(crate) fn count_credit(&mut self, now: i64) {
        self.roll_counters_day(now);
        self.credited_today += 1;
    }

    /// The day's probes and credits as of `now`, zero once the UTC day they count has passed: a recipe that neither
    /// probed nor earned a credit since midnight reads the new day without waiting for either event.
    pub(crate) fn counts_today(&self, now: i64) -> (u64, u64) {
        if self.counters_day == utc_day(now) {
            (self.probes_today, self.credited_today)
        } else {
            (0, 0)
        }
    }

    /// Every state write. A move to any state but `Parked` ends the park, so a stale park never outlives it.
    pub(crate) fn set_state(&mut self, state: AgentState) {
        self.state = state;
        if state != AgentState::Parked {
            self.park_kind = None;
            self.wake_at = None;
            self.deadline = None;
            self.park_began = None;
        }
    }

    /// Drops the part of a park whose return has come without a request: a deadline the harness stopped the command
    /// at, or a wake that passed. True when nothing is left of the park, which is when it ended.
    pub(crate) fn pass_park(&mut self, now: i64) -> bool {
        let Some(kind) = self.park_kind else {
            return false;
        };
        let mut kind = Some(kind);
        if self.deadline.is_some_and(|deadline| deadline <= now) {
            self.deadline = None;
            kind = match kind {
                Some(ParkKind::Background) => None,
                Some(ParkKind::Both) => Some(ParkKind::Scheduled),
                other => other,
            };
        }
        if self.wake_at.is_some_and(|wake| wake <= now) {
            self.wake_at = None;
            kind = match kind {
                Some(ParkKind::Scheduled) => None,
                Some(ParkKind::Both) => Some(ParkKind::Background),
                other => other,
            };
        }
        self.park_kind = kind;
        kind.is_none()
    }
}

/// What the ceilings decide for one retained recipe at a tick: probe on, hold the recipe with a reason, or end it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Probe,
    Hold(Reason),
    End(Reason),
}

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
    pub model: String,
    /// The covering recipe's speed, which the credited request's has to match.
    pub fast: bool,
    pub real_at: i64,
    pub ttl_ms: i64,
    pub until: Option<i64>,
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
    /// The kind of the account the current attempt was placed on, read off `AccountSelected`, so a skip and a kept-warm
    /// credit file under the funding this request ran under whatever the registry holds by then.
    pub kind: Option<AccountKind>,
    /// Whether the current attempt's head reported overage in use, the fact the request's own row files under.
    pub in_overage: bool,
}

/// One unanswered probe with the label that settles it as wasted. Queued under the lock and written to the ledger by
/// the engine once the lock is released.
pub(crate) struct Settlement {
    pub key: SessionKey,
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub account: Option<String>,
    pub label: Wasted,
    /// One probe's cost: a settlement is one wasted probe, so the rollup files it under its own funding.
    pub cost: f64,
    /// The stopped recipe carried `speed: "fast"`, so the settlement files under speed `fast`.
    pub fast: bool,
    /// The kind the probe was served under and whether its answer reported overage, as the probe's own cost row
    /// took them, so the waste files under the same funding word whatever the registry holds now.
    pub kind: Option<AccountKind>,
    pub in_overage: bool,
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
    /// The kind and the overage of the request the recipe came from, off its observation, so the skip files under the
    /// funding that request ran under.
    pub kind: Option<AccountKind>,
    pub in_overage: bool,
}

impl Skipped {
    fn of(key: &SessionKey, agent: &Agent, state: RecipeState) -> Skipped {
        Skipped {
            key: key.clone(),
            key_generation: agent.key_generation,
            pool_id: agent.pool_id.clone(),
            account: agent.account.clone(),
            canonical_model: agent.canonical_model.clone(),
            fast: agent.current.as_ref().is_some_and(|o| o.recipe.fast),
            state,
            kind: agent.current.as_ref().and_then(|o| o.kind),
            in_overage: agent.current.as_ref().is_some_and(|o| o.in_overage),
        }
    }
}

/// One probe the next real request has not answered yet: what it cost and the unit it settled in, the lineage a
/// request has to match to be read as its answer, and the end of the entry it wrote, which settles it on its own.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UnansweredProbe {
    /// The relay's request id of the probe's exchange, which the probe's own ledger request row carries, so a
    /// settlement of this probe can be told from a repeat of it.
    pub id: String,
    pub cost: f64,
    pub unit: Option<SavingsUnit>,
    pub account: Option<String>,
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
    /// The kind the probe was served under and whether its answer reported overage, off its `ProbeRecord`, so its
    /// settlement files under the funding its cost row took.
    pub kind: Option<AccountKind>,
    pub in_overage: bool,
}

/// One unanswered probe with the rollup keys it was paid under, which its settlement files under.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Kept {
    pub key_generation: i64,
    pub pool_id: String,
    pub canonical_model: String,
    pub account: Option<String>,
    pub fast: bool,
    pub cost: f64,
    /// When the entry this probe wrote expires: a read arriving at or past it found the entry gone.
    pub entry_expires: i64,
    pub kind: Option<AccountKind>,
    pub in_overage: bool,
    /// What the probe proved, carried from its `UnansweredProbe` by `Kept::of`, so a turn that arrives after the
    /// ceiling moved the probe here reads the evidence the unanswered list held.
    pub covered: bool,
    pub unit: Option<SavingsUnit>,
    /// The label a probe written off at a ceiling settles under when its entry lapses with no turn: `idle_limit`
    /// today. A turn that reads the probe leaves the label in place, so one that ends without deciding the probe
    /// hands it back to the lapse sweep. `None` for a probe a real request superseded, which waits for that
    /// request's completion.
    pub pending: Option<Wasted>,
}

impl Kept {
    pub(crate) fn of(agent: &Agent, probe: &UnansweredProbe) -> Kept {
        Kept {
            key_generation: agent.key_generation,
            pool_id: agent.pool_id.clone(),
            canonical_model: probe.model.clone(),
            account: probe.account.clone(),
            fast: probe.fast,
            cost: probe.cost,
            entry_expires: probe.expires,
            kind: probe.kind,
            in_overage: probe.in_overage,
            covered: probe.covered,
            unit: probe.unit,
            pending: None,
        }
    }

    pub(crate) fn settle(self, key: &SessionKey, label: Wasted) -> Settlement {
        Settlement {
            key: key.clone(),
            key_generation: self.key_generation,
            pool_id: self.pool_id,
            canonical_model: self.canonical_model,
            account: self.account,
            label,
            cost: self.cost,
            fast: self.fast,
            kind: self.kind,
            in_overage: self.in_overage,
        }
    }
}

/// What ending an observation does with the probes the recipe leaves unanswered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// A labeled stop settles them and the kept ones under its label; a stop with no label drops them and leaves
    /// the kept ones for whatever decides them.
    Settle,
    /// A real request supersedes the recipe and keeps them until its completion decides them, or the idle limit
    /// keeps them pending under its label until their entry lapses or a turn reads them.
    Keep,
    /// A bus gap or a failed request ends the observation without deciding the probes sent while it was open: they
    /// wait where they are, with the recipe's speed, for the next request.
    Leave,
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
    pub settlements: Vec<Settlement>,
    /// The settlements the load queued, held until a save has dropped their probes from the agents: a failed save
    /// then sends nothing to the ledger, and a restart before a landed save settles each probe once rather than twice.
    pub settlements_after_save: Vec<Settlement>,
    /// The probes a gate withheld this locked section, with the keys their skip rows file under, read while the
    /// recipe still held its speed.
    pub skipped: Vec<Skipped>,
    /// Bus gaps seen by the observer; each one discarded every observation that was still pending.
    pub bus_gaps: u64,
    /// Events a gap left queued behind it, counted down as they are applied. What arrives inside that
    /// backlog cannot be trusted: the terminal event of any request among them may be one the gap dropped.
    pub gap_backlog: usize,
    pub next_incarnation: u64,
    /// The engine's timing, copied onto every agent it creates.
    pub timing: KeepaliveTiming,
    /// The idle-age counters by pool id. Never pruned while the engine runs, so a pool that leaves keeps the counts
    /// it already took until the restart.
    pub idle_returns: BTreeMap<String, IdleReturns>,
    /// The rows changed or removed since their last acknowledged save, each under the revision of its latest change.
    pub dirty: Dirty,
    /// Bumped by every touch, so a save that lands after a newer change still knows which change it carried.
    pub revision: u64,
    /// Whether the last save failed, so a failing registry is logged once per streak rather than every flush.
    pub persist_failing: bool,
    /// The accounts whose last tier read failed, so a registry that stays unreadable is logged once per account and
    /// streak rather than at every probe it settles. Held in memory alone, so a restart starts each streak over.
    pub unreadable_tiers: BTreeSet<String>,
    /// The request events and reports refused after the quiesce.
    pub late_updates: u64,
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
    /// Orders the flushes: a snapshot, its save and its acknowledgment run under it whole, so two flushes never
    /// overlap and an older batch never reaches the store after a newer one.
    pub(crate) flush_lock: Mutex<()>,
    /// How many flushes have entered, counted before the flush lock, so a test can hold a save and know a second
    /// flush is queued behind it. Cheap enough to keep in every build, so only the reader is gated.
    pub(crate) persist_entries: AtomicU64,
    /// How many flushes have read their batch, for a test to change a row only once the save it races has read it.
    /// Cheap enough to keep in every build, so only the reader is gated.
    pub(crate) persist_snapshots: AtomicU64,
    /// Woken as either count rises, which is what a test's wait on them waits on.
    pub(crate) persist_progress: Notify,
}

impl Engine {
    pub fn new(parts: EngineParts) -> Arc<Engine> {
        // Every read of a stored policy the bounds now refuse answers keepalive off; the operator learns which pool
        // and why once, here, rather than from a tick that reads it every second.
        for pool in parts.router.snapshot().pools() {
            if let Err(refusal) = KeepalivePolicy::read(&pool.keepalive_json) {
                tracing::warn!(
                    pool = %pool.id,
                    %refusal,
                    "a pool's stored keepalive policy is refused and reads as keepalive off"
                );
            }
        }
        Arc::new(Engine {
            inner: Mutex::new(Inner::empty(parts.timing)),
            parts,
            cancel: CancellationToken::new(),
            idle: Notify::new(),
            workers: TaskTracker::new(),
            router_view_calls: AtomicU64::new(0),
            flush_lock: Mutex::new(()),
            persist_entries: AtomicU64::new(0),
            persist_snapshots: AtomicU64::new(0),
            persist_progress: Notify::new(),
        })
    }

    /// Compiled only under the `debug-hooks` feature, which a dependent's dev-dependency turns on, so a
    /// release build carries no way to count the scheduler's own router traffic.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn router_views(&self) -> u64 {
        self.router_view_calls
            .load(std::sync::atomic::Ordering::Relaxed)
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

    /// The counters per pool id, for the overview and the flush.
    pub fn idle_returns(&self) -> BTreeMap<String, IdleReturns> {
        self.lock().idle_returns.clone()
    }

    pub fn stats(&self) -> EngineStats {
        let inner = self.lock();
        EngineStats {
            agents: inner.agents,
            bus_gaps: inner.bus_gaps,
            late_updates: inner.late_updates,
            live_sessions: inner.sessions.values().filter(|s| !s.ended).count(),
            retained_bytes: inner.retained,
            running_probes: inner.running,
            sessions: inner.sessions.len(),
        }
    }

    /// The pool's ceilings resolved over its fundings, its main TTLs and its models, and `None` for a pool with no
    /// member.
    /// `accounts` are the pool's members' accounts: an OAuth member brings the plan's factors, an API-key
    /// member the dollar rows. The router's catalog names the dated id each undated dollar row resolves to, and a
    /// router that cannot answer leaves every row judged under its own id.
    pub async fn resolve_pool_ceilings(
        &self,
        pool: &PoolRow,
        accounts: &[AccountRow],
    ) -> Option<ResolvedCeilings> {
        let mut fundings: Vec<Funding> = Vec::new();
        for account in accounts {
            let funding = Funding::of(account.kind, false);
            if !fundings.contains(&funding) {
                fundings.push(funding);
            }
        }
        let rows = FactorRows::of(&self.parts.pricing.current(), &self.parts.tables);
        let undated: Vec<String> = rows
            .dollar_models()
            .filter(|model| listing::undated(model).is_none())
            .map(str::to_string)
            .collect();
        let dated: HashMap<String, String> = self
            .parts
            .router
            .canonicals(undated.clone())
            .await
            .map(|answered| {
                undated
                    .into_iter()
                    .zip(answered)
                    .filter(|(model, dated)| model != dated)
                    .collect()
            })
            .unwrap_or_default();
        resolve_pool(
            &KeepalivePolicy::from_json(&pool.keepalive_json),
            &CacheTtlPolicy::from_json(&pool.cache_ttl_json),
            &pool.model_rules,
            &fundings,
            &rows,
            &dated,
            &self.parts.timing,
        )
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
        }
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
    pub(crate) fn empty(timing: KeepaliveTiming) -> Inner {
        Inner {
            sessions: BTreeMap::new(),
            pending: HashMap::new(),
            reasons: BTreeMap::new(),
            retained: 0,
            agents: 0,
            running: 0,
            closed: false,
            settlements: Vec::new(),
            settlements_after_save: Vec::new(),
            skipped: Vec::new(),
            bus_gaps: 0,
            gap_backlog: 0,
            next_incarnation: 0,
            timing,
            idle_returns: BTreeMap::new(),
            dirty: Dirty::default(),
            revision: 0,
            persist_failing: false,
            late_updates: 0,
            unreadable_tiers: BTreeSet::new(),
        }
    }

    /// The one writer of an agent's reason, so the counter behind the views never disagrees with the reason
    /// they show. Nearly every change to a recipe writes a reason, so this is where most of them become dirty.
    pub(crate) fn set_reason(&mut self, key: &SessionKey, agent_id: &str, reason: Reason) {
        if let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        {
            agent.reason = reason;
        }
        *self.reasons.entry(reason).or_default() += 1;
        self.touch_agent(key, agent_id);
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
        self.end_observation(key, agent_id, wasted, Unanswered::Settle);
        self.set_reason(key, agent_id, reason);
    }

    /// `discard` for a real request that supersedes the recipe: the probes the recipe leaves unanswered were paying
    /// for that request, so it keeps them until the first of these decides them: its completion, once it reached
    /// the upstream (each probe on its own: a warm return on its lineage inside its entry clears it, one past its
    /// entry settles it under `expired`, and any other return, on another lineage or with no read, under
    /// `cold_return`), a labeled discard (under its label), or the agent's lapse or eviction. A request the engine
    /// takes no observation of, or one refused or cancelled before the upstream answered, leaves them kept for
    /// whichever comes next.
    pub(crate) fn discard_keeping_unanswered(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
    ) {
        self.end_observation(key, agent_id, None, Unanswered::Keep);
        self.set_reason(key, agent_id, reason);
    }

    /// The decision `discard_keeping_unanswered` defers, taken by the completion of the request that kept the
    /// probes, `read` being what the completion read from the cache. Each probe answers to the lineage it was paid
    /// under and to the entry it wrote, since a request that failed before any head leaves its probes behind with
    /// the agent already moved to that request's model and entry: a warm return is one on the probe's model, speed
    /// and account, arriving before that probe's entry expired, that read the cache. A kept-warm credit takes no
    /// shortcut here: a probe off the lineage the credited request read kept nothing it read. The model is the
    /// probe's as resolutions followed it. A probe sent while the request was open is not among them: it pays for
    /// the request after this one.
    pub(crate) fn decide_unanswered(&mut self, key: &SessionKey, agent_id: &str, read: u64) {
        self.touch_agent(key, agent_id);
        let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        else {
            return;
        };
        let fast = agent.current.as_ref().is_some_and(|o| o.recipe.fast);
        // The read happened when the request reached the cache, so a stream that outlives the entry still read it.
        let arrived = agent.current.as_ref().map(|o| o.started);
        for kept in std::mem::take(&mut agent.kept) {
            let lineage = kept.canonical_model == agent.canonical_model
                && kept.fast == fast
                && kept.account == agent.account;
            // A probe whose entry was gone when the request arrived kept nothing the request could read, whatever it
            // read: `expired` is its own label, so a credited request never sits beside a cold return it
            // contradicts.
            let label = match arrived {
                Some(at) if at >= kept.entry_expires => Some(Wasted::Expired),
                Some(_) if read > 0 && lineage => None,
                _ => Some(Wasted::ColdReturn),
            };
            if let Some(label) = label {
                self.settlements.push(kept.settle(key, label));
            }
        }
    }

    /// `discard`'s bookkeeping without writing a reason: for a terminal state that is already the agent's
    /// current reason, so ending the observation does not count that reason a second time.
    pub(crate) fn end_observation(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        wasted: Option<Wasted>,
        unanswered: Unanswered,
    ) {
        self.touch_agent(key, agent_id);
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
        if unanswered == Unanswered::Leave {
            return;
        }
        let probes = std::mem::take(&mut agent.unanswered);
        if unanswered == Unanswered::Keep {
            // A row apiece: a request whose completion never came leaves its probes for the next one, which may
            // keep probes paid under other keys, and each settles by the entry it wrote. A ceiling's label rides
            // along, for the lapse sweep to settle under when no turn reads them first.
            let kept: Vec<Kept> = probes
                .iter()
                .map(|probe| Kept {
                    pending: wasted,
                    ..Kept::of(agent, probe)
                })
                .collect();
            agent.kept.extend(kept);
            return;
        }
        let Some(label) = wasted else {
            return;
        };
        for probe in &probes {
            self.settlements
                .push(Kept::of(agent, probe).settle(key, label));
        }
        for kept in std::mem::take(&mut agent.kept) {
            self.settlements.push(kept.settle(key, label));
        }
    }

    /// Settles every kept probe whose `pending` label is set and whose `entry_expires <= now` under that label, on
    /// each agent with no turn open, and touches each agent it settled for the next save. True when it queued a
    /// settlement.
    pub(crate) fn settle_lapsed_pending(&mut self, now: i64) -> bool {
        let lapsed = |kept: &Kept| kept.pending.is_some() && kept.entry_expires <= now;
        let open = &self.pending;
        let mut settled: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            for (agent_id, agent) in session.agents.iter_mut() {
                // An open turn's completion decides the probes, as `lapse` waits for it. A turn that ends with no
                // answer or at a bus gap decides nothing and leaves them here.
                let turn_open = agent
                    .current
                    .as_ref()
                    .is_some_and(|o| open.contains_key(&o.id));
                if turn_open || !agent.kept.iter().any(lapsed) {
                    continue;
                }
                let (due, waiting): (Vec<Kept>, Vec<Kept>) = std::mem::take(&mut agent.kept)
                    .into_iter()
                    .partition(lapsed);
                agent.kept = waiting;
                for kept in due {
                    if let Some(label) = kept.pending {
                        self.settlements.push(kept.settle(key, label));
                    }
                }
                settled.push((key.clone(), agent_id.clone()));
            }
        }
        for (key, agent_id) in &settled {
            self.touch_agent(key, agent_id);
        }
        !settled.is_empty()
    }

    /// A gate's hold on a retained recipe. One hold withholds one probe, whatever number of ticks it spans: the
    /// production ticker runs each second, so a five-hour cooldown would otherwise ledger 18,000 withheld probes
    /// for the one it actually withheld. A hold that begins before the due time has withheld nothing yet, so the
    /// skip waits for the due time to pass. The due time is read after the policy refresh, so the pool's current
    /// policy decides whether a probe was due, and a probe already in flight was not withheld.
    pub(crate) fn hold(&mut self, key: &SessionKey, agent_id: &str, reason: Reason, now: i64) {
        let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        else {
            return;
        };
        let began = agent.reason != reason;
        let counted = agent.hold_counted && !began;
        let was_due = !agent.in_flight && agent.due_at().is_some_and(|at| at <= now);
        // A hold spans every tick it lasts, so only a change is written: a held recipe would otherwise rewrite its
        // sealed prompt at every flush.
        let moved = agent.hold_counted != (counted || was_due);
        agent.hold_counted = counted || was_due;
        if !counted && was_due {
            let state = recipe_state(reason, agent.paused);
            self.skipped.push(Skipped::of(key, agent, state));
        }
        if began {
            self.set_reason(key, agent_id, reason);
        } else if moved {
            self.touch_agent(key, agent_id);
        }
    }

    /// The probes sent since the last real request, settled under `label` the way a discard settles them, with the
    /// recipe kept: a hold that retains the prompt still writes off what it paid for.
    pub(crate) fn settle_unanswered(&mut self, key: &SessionKey, agent_id: &str, label: Wasted) {
        let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        else {
            return;
        };
        let probes = std::mem::take(&mut agent.unanswered);
        if probes.is_empty() {
            return;
        }
        for probe in &probes {
            self.settlements
                .push(Kept::of(agent, probe).settle(key, label));
        }
        self.touch_agent(key, agent_id);
    }

    /// The entry is dead at its expiry and the lead already places the due time ahead of it, so a probe past it warms
    /// nothing: the recipe ends under `expired` rather than being probed, held or not, its unanswered probes wasted,
    /// and the next real request re-arms it. A probe already in flight was sent in time and settles on its own answer,
    /// a hold that already ledgered the probe it withheld is not counted again, and the skip takes the recipe's speed
    /// before the discard drops the recipe. While the request the observation follows is still open, the recipe is
    /// held unprobed instead: its completion earns its credit and decides the probes it kept, and the next tick lapses
    /// it. A paused recipe loses its prompt alone (`drop_paused_prompt`). True when the tick is done with the recipe.
    fn lapse(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
        lapsed: &mut Vec<Skipped>,
    ) -> bool {
        let agent = &self.sessions[key].agents[agent_id];
        let Some(expires) = agent
            .expires
            .filter(|expires| !agent.in_flight && now >= *expires)
        else {
            return false;
        };
        if agent
            .current
            .as_ref()
            .is_some_and(|o| self.pending.contains_key(&o.id))
        {
            return true;
        }
        if agent.paused {
            tracing::info!(key = %key.0, agent = %agent_id, expires, late_ms = now - expires,
                "a paused keepalive recipe's entry expired, so its prompt was dropped");
            self.drop_paused_prompt(key, agent_id);
            return true;
        }
        tracing::info!(key = %key.0, agent = %agent_id, expires, late_ms = now - expires,
            "a keepalive recipe lapsed before its probe, so the probe was skipped");
        if !agent.hold_counted {
            // The lapse is what withholds the probe, so it counts under the state the discard below leaves.
            lapsed.push(Skipped::of(
                key,
                agent,
                recipe_state(Reason::Expired, agent.paused),
            ));
        }
        self.discard(key, agent_id, Reason::Expired, Some(Wasted::Expired));
        true
    }

    /// The lapse of a paused recipe. The pause withheld every probe and holds until the next real request, so the agent
    /// keeps its pause bit and its reason for the view's `paused`, and only the prompt the cold entry made worthless
    /// leaves, with the probes it paid for settled under `expired`. No probe was due, so no skip is ledgered.
    fn drop_paused_prompt(&mut self, key: &SessionKey, agent_id: &str) {
        self.touch_agent(key, agent_id);
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
        agent.last_stop = Some(Wasted::Expired);
        for probe in std::mem::take(&mut agent.unanswered) {
            self.settlements
                .push(Kept::of(agent, &probe).settle(key, Wasted::Expired));
        }
        for kept in std::mem::take(&mut agent.kept) {
            self.settlements.push(kept.settle(key, Wasted::Expired));
        }
    }

    /// A deadline or a wake that passed ends that part of the park, and a park with nothing left ended without a
    /// request: the idle window restarts here and main reads idle. The mod reports the same end with `idle` when its
    /// heartbeat sees it first, and whichever comes first records it. Main's end is the engine's own move out of
    /// parked, which no report repeats.
    pub(crate) fn end_passed_park(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let Some(agent) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(agent_id))
        else {
            return;
        };
        let park = (agent.park_kind, agent.deadline, agent.wake_at);
        if !agent.pass_park(now) {
            if park != (agent.park_kind, agent.deadline, agent.wake_at) {
                self.touch_agent(key, agent_id);
            }
            return;
        }
        // The window restarts here, so the one counted so far ends with no return, closed before its start moves.
        agent.close_idle_episode();
        agent.idle_window_floor = None;
        agent.park_ended_at = Some(now);
        if agent.id != MAIN_AGENT {
            agent.park_began = None;
            self.touch_agent(key, agent_id);
            return;
        }
        agent.set_state(AgentState::Idle);
        self.touch_agent(key, agent_id);
    }

    /// Advances main's idle window on every attended session: opens one where none is counted and raises `reached`
    /// up to the window's current bucket. It reads the sessions, never the retained recipes, so a session whose
    /// recipe a ceiling discarded, or that never sent a request, still reaches the later buckets. A busy main keeps
    /// its window and counts nothing, and a window whose return a prompt announced waits for the request.
    pub(crate) fn advance_idle_episodes(&mut self, snapshot: &RegistrySnapshot, now: i64) {
        let mut opened: Vec<SessionKey> = Vec::new();
        let mut raised: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            if session.ended || !session.attended {
                continue;
            }
            let Some(main) = session.agents.get_mut(MAIN_AGENT) else {
                continue;
            };
            if matches!(
                main.state,
                AgentState::Busy | AgentState::Released | AgentState::Unknown
            ) {
                continue;
            }
            if open_idle_episode(main, snapshot) {
                opened.push(key.clone());
            }
            let Some(episode) = main.idle_episode.as_mut() else {
                continue;
            };
            if episode.prompt_at.is_some() {
                continue;
            }
            let bucket = idle_age_bucket(now - episode.started);
            // Only a window that crossed into a new bucket is written, so an idle session costs no save per tick.
            if episode.bucket.is_none_or(|counted| bucket > counted) {
                raised.push((key.clone(), episode.pool_key.clone()));
            }
            raise_reached(
                self.idle_returns
                    .entry(episode.pool_key.clone())
                    .or_default(),
                episode.bucket,
                bucket,
            );
            episode.bucket = Some(episode.bucket.map_or(bucket, |counted| counted.max(bucket)));
        }
        for key in opened {
            self.touch_agent(&key, MAIN_AGENT);
        }
        for (key, pool) in raised {
            self.touch_agent(&key, MAIN_AGENT);
            self.touch_pool(&pool);
        }
    }

    /// Main's request ends its open idle window: the buckets the window crossed since the last tick count as reached,
    /// and the bucket of its age counts a return when the request continues the prefix the window opened on. The age
    /// runs to the prompt that announced the request, when one did, since the user came back then however many ticks
    /// pass before the request arrives.
    pub(crate) fn end_idle_episode(&mut self, key: &SessionKey, boundaries: &[String], now: i64) {
        let Some(main) = self
            .sessions
            .get_mut(key)
            .and_then(|s| s.agents.get_mut(MAIN_AGENT))
        else {
            return;
        };
        let Some(episode) = main.close_idle_episode() else {
            return;
        };
        // Never below the fence: the window already counted as reaching it, so its return is read there.
        let bucket = idle_age_bucket(episode.prompt_at.unwrap_or(now) - episode.started)
            .max(episode.bucket.unwrap_or(0));
        let counters = self
            .idle_returns
            .entry(episode.pool_key.clone())
            .or_default();
        let raised = episode.bucket.is_none_or(|counted| bucket > counted);
        raise_reached(counters, episode.bucket, bucket);
        let anchor = episode
            .anchor
            .as_ref()
            .map(|(static_hash, tail)| (static_hash.as_str(), tail.as_str()));
        let returned = continues_prefix(anchor, boundaries);
        if returned {
            counters.buckets[bucket].returned += 1;
        }
        self.touch_agent(key, MAIN_AGENT);
        if raised || returned {
            self.touch_pool(&episode.pool_key);
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
        let mut followed: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            for (agent_id, agent) in session.agents.iter_mut() {
                let mut moved = false;
                if let Some(dated) = canonical.get(&agent.canonical_model) {
                    agent.canonical_model = dated.clone();
                    moved = true;
                }
                if let Some(prior) = agent.current.as_mut().and_then(|o| o.prior.as_mut()) {
                    if let Some(dated) = canonical.get(&prior.model) {
                        prior.model = dated.clone();
                        moved = true;
                    }
                }
                for kept in &mut agent.kept {
                    if let Some(dated) = canonical.get(&kept.canonical_model) {
                        kept.canonical_model = dated.clone();
                        moved = true;
                    }
                }
                for probe in &mut agent.unanswered {
                    if let Some(dated) = canonical.get(&probe.model) {
                        probe.model = dated.clone();
                        moved = true;
                    }
                }
                if moved {
                    followed.push((key.clone(), agent_id.clone()));
                }
            }
        }
        for (key, agent_id) in followed {
            self.touch_agent(&key, &agent_id);
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

    /// Makes room under `cap` for a recipe of `size` by evicting retained recipes: unattended sessions' agents
    /// first, then attended sessions' idle agents, each oldest activity first. An agent with a probe in flight or a
    /// request still open is never evicted. When even every candidate would not make room nothing is evicted and
    /// the answer is false, so the refusal that follows costs no recipe.
    pub(crate) fn evict_for(&mut self, cap: usize, size: usize) -> bool {
        let mut candidates: Vec<(bool, i64, SessionKey, String, usize)> = Vec::new();
        for (key, session) in &self.sessions {
            for (agent_id, agent) in &session.agents {
                let Some(observation) = agent.current.as_ref() else {
                    continue;
                };
                if (!session.attended || agent.state == AgentState::Idle)
                    && !agent.in_flight
                    && !self.pending.contains_key(&observation.id)
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
            return false;
        }
        for (key, agent_id) in evicted {
            self.discard(&key, &agent_id, Reason::Evicted, Some(Wasted::Evicted));
        }
        true
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
                // A lapse is no return, so main's window ends here.
                if let Some(main) = session.agents.get_mut(MAIN_AGENT) {
                    main.close_idle_episode();
                }
            }
            self.touch_session(&key);
            self.touch_agent(&key, MAIN_AGENT);
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
            self.forget_session(&key);
            self.sessions.remove(&key);
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
            self.forget_session(&key);
            self.sessions.remove(&key);
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
    pub counters_day: i64,
    pub credited_today: u64,
    pub deadline: Option<i64>,
    pub failures: u32,
    pub idle_ticks: u64,
    pub in_flight: bool,
    pub last_activity: i64,
    pub migrations: u64,
    pub next_probe_at: Option<i64>,
    pub park_kind: Option<ParkKind>,
    pub paused: bool,
    pub probe_usage: ProbeUsage,
    pub probes: u64,
    pub probes_today: u64,
    pub reason: Reason,
    pub request_generation: u64,
    pub state: AgentState,
    pub unanswered: usize,
    pub wake_at: Option<i64>,
}

/// The engine's bus task. Every event is handed to `observe` under the lock; nothing here awaits while holding
/// it, so a slow registry read never stalls the queue. The bus drops for a subscriber that falls behind and
/// counts it; a rise in that counter means events this task never saw, so every observation still open is
/// discarded before the next event is applied.
pub fn spawn_observer(engine: Arc<Engine>) -> tokio::task::JoinHandle<()> {
    // Best-effort: a drop reads as a gap below, which discards the observations it could have misled.
    let subscriber = engine
        .parts
        .bus
        .subscribe("keepalive", Delivery::BestEffort);
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
            "the keepalive subscriber fell behind the bus, so the pending observations were discarded"
        );
        let open: Vec<(SessionKey, String)> = inner.pending.values().cloned().collect();
        for (key, agent_id) in open {
            inner.end_observation(&key, &agent_id, None, Unanswered::Leave);
            inner.set_reason(&key, &agent_id, Reason::BusGap);
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
            counters_day: agent.counters_day,
            credited_today: agent.credited_today,
            deadline: agent.deadline,
            failures: agent.failures,
            idle_ticks: agent.idle_ticks,
            in_flight: agent.in_flight,
            last_activity: agent.last_activity,
            migrations: agent.migrations,
            next_probe_at: agent.next_probe_at(),
            park_kind: agent.park_kind,
            paused: agent.paused,
            probe_usage: agent.usage,
            probes: agent.probes,
            probes_today: agent.probes_today,
            reason: agent.reason,
            request_generation: agent.generation,
            state: agent.state,
            unanswered: agent.unanswered.len() + agent.kept.len(),
            wake_at: agent.wake_at,
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
        let mut inner = self.lock();
        let key: SessionKey = (key_id, session_id);
        if inner.closed {
            // A request on a session the engine holds is one the last save can no longer carry.
            if inner.sessions.contains_key(&key) {
                inner.late_updates += 1;
            }
            return;
        }
        inner.expire(now, expiry);
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
        // The hijack guard: an auxiliary or foreign request must not touch the recipe, the schedule, or the
        // unanswered probes a re-arming request's credit depends on. `reason` stays the recipe's gating
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
        // One touch for everything below: the lock is held to the end, so the flush reads the agent as this request
        // leaves it.
        inner.touch_agent(&key, &agent_id);
        // Ahead of the return below, so a foreign request on a new prefix ends the window too. The return is judged
        // against the prefix the window opened on, never on the verdict, which re-arms on any chain once a ceiling
        // has discarded the recipe.
        if agent_id == MAIN_AGENT && verdict != Guard::Auxiliary {
            inner.end_idle_episode(&key, &observed.boundaries, now);
        }
        if matches!(verdict, Guard::Auxiliary | Guard::Foreign) {
            return;
        }
        // Apart from the recipe, which keepalive off, an opt-out or a refused body leaves unretained: the next idle
        // window judges a return against this request's prefix either way. A request without both hashes leaves none.
        if agent_id == MAIN_AGENT {
            inner
                .sessions
                .get_mut(&key)
                .unwrap()
                .agents
                .get_mut(&agent_id)
                .unwrap()
                .last_prefix = observed
                .static_hash
                .clone()
                .zip(observed.boundaries.last().cloned());
        }
        // What the previous recipe proved, taken before anything below overwrites it and consumed with the
        // observation: the kept-warm credit reads this and nothing on the agent. The newest unanswered probe wrote
        // the entry this request can read, a rewrite included, so its coverage is the one taken, and it leaves with
        // the probes this request keeps. A recipe the idle limit ended left its probes kept and pending, so the
        // newest of those whose entry still lives stands in for it.
        let prior = {
            let agent = &inner.sessions[&key].agents[&agent_id];
            let newest_kept = agent
                .kept
                .iter()
                .rev()
                .find(|kept| kept.pending.is_some() && now < kept.entry_expires);
            agent.last_real_at.map(|real_at| {
                let ttl_ms = agent.ttl_ms;
                match (agent.unanswered.last(), newest_kept) {
                    (Some(probe), _) => PriorCoverage {
                        account: probe.account.clone(),
                        covered: probe.covered,
                        unit: probe.unit,
                        model: probe.model.clone(),
                        fast: probe.fast,
                        real_at,
                        ttl_ms,
                        until: Some(probe.expires),
                    },
                    (None, Some(kept)) => PriorCoverage {
                        account: kept.account.clone(),
                        covered: kept.covered,
                        unit: kept.unit,
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
                        model: agent.canonical_model.clone(),
                        fast: false,
                        real_at,
                        ttl_ms,
                        until: None,
                    },
                }
            })
        };
        let poisoned = inner.gap_backlog > 0;
        inner.discard_keeping_unanswered(&key, &agent_id, Reason::AwaitingSelection);
        inner.sessions.get_mut(&key).unwrap().last_real_at = Some(now);
        inner.touch_session(&key);
        let agent = inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap();
        if agent.state == AgentState::Unknown {
            inner.set_reason(&key, &agent_id, Reason::AgentUnknown);
            return;
        }
        agent.set_state(AgentState::Busy);
        agent.last_activity = now;
        // The request restarts main's idle window at its own instant, which can be the instant of the prompt that set
        // the floor at the close, so the floor goes with it. A foreign request, an auxiliary one and one on an
        // `unknown` main returned above and move no start, so their floor stands.
        agent.idle_window_floor = None;
        agent.cache_started = now;
        agent.idle_ticks = 0;
        agent.cold = 0;
        agent.failures = 0;
        agent.hold_counted = false;
        agent.last_real_at = Some(now);
        agent.canonical_model = observed.canonical_model.clone();
        agent.key_generation = observed.key_generation;
        if inner.pending.len() >= limits.max_agents {
            inner.set_reason(&key, &agent_id, Reason::RequestLimit);
            return;
        }
        let mut recipe = match make_recipe(
            &observed.headers,
            &observed.body,
            &observed.model,
            self.parts.max_request_bytes,
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
        if inner.retained + recipe.size > limits.max_retained_bytes
            && !inner.evict_for(limits.max_retained_bytes, recipe.size)
        {
            inner.set_reason(&key, &agent_id, Reason::StorageLimit);
            return;
        }
        inner.retained += recipe.size;
        let agent = inner
            .sessions
            .get_mut(&key)
            .unwrap()
            .agents
            .get_mut(&agent_id)
            .unwrap();
        agent.current = Some(Observation {
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
            kind: None,
            in_overage: false,
        });
        agent.body_dirty = true;
        // The mark is retired only by a save at the row's revision, so the new body takes a revision of its own.
        inner.touch_agent(&key, &agent_id);
        inner.pending.insert(observed.request_id, (key, agent_id));
    }

    fn on_selected(&self, selected: AccountSelected) {
        let snapshot = self.parts.router.snapshot();
        let mut inner = self.lock();
        let Some((key, agent_id)) = inner.pending.get(&selected.request_id).cloned() else {
            return;
        };
        // Refused after the quiesce as the completion is, and counted: `live` would refuse it uncounted.
        if inner.closed {
            inner.late_updates += 1;
            return;
        }
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
        observation.kind = kind_of_word(&selected.account_kind);
        observation.in_overage = false;
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
        } else if snapshot.account(&selected.account_id).is_none() {
            Some(Reason::IneligibleCredential)
        } else {
            None
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
        if inner.closed {
            inner.late_updates += 1;
            return;
        }
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
            if observation.attempt == head.attempt {
                let in_overage = head.headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("anthropic-ratelimit-unified-overage-in-use")
                        && value.trim().eq_ignore_ascii_case("true")
                });
                let moved = observation.in_overage != in_overage;
                observation.in_overage = in_overage;
                // A failed attempt is not evidence about the cache, and the next selection re-arms the attempt.
                if !(200..300).contains(&head.status) {
                    observation.ready = false;
                    inner.touch_agent(&key, &agent_id);
                } else if moved {
                    inner.touch_agent(&key, &agent_id);
                }
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
        if inner.closed {
            inner.late_updates += 1;
            return;
        }
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
        // After the quiesce the last save holds the agent as it stood then, so a completion that lands later earns no
        // credit and arms nothing. The relay has already answered the client either way.
        if inner.closed {
            inner.late_updates += 1;
            return;
        }
        inner.touch_agent(&key, &agent_id);
        let read = complete
            .usage
            .as_ref()
            .and_then(|u| u.cache_read_tokens)
            .unwrap_or(0);
        let reason = match complete.outcome {
            Outcome::Succeeded => None,
            Outcome::Canceled => Some(Reason::RequestCanceled),
            Outcome::Failed | Outcome::Exhausted | Outcome::Incomplete => {
                Some(Reason::RequestFailed)
            }
            Outcome::Rejected => Some(Reason::RequestRejected),
        };
        if let Some(reason) = reason {
            // Only an answer from the upstream says anything about the cache the kept probes held; a request
            // refused or cancelled before one arrived leaves them for the next request.
            if complete.ttfb_ms.is_some() {
                inner.decide_unanswered(&key, &agent_id, read);
            }
            // A probe sent while this request was open pays for the request after it, whatever this one did.
            inner.end_observation(&key, &agent_id, None, Unanswered::Leave);
            inner.set_reason(&key, &agent_id, reason);
            return;
        }
        inner.pending.remove(&complete.request_id);
        // Read from the observation's own snapshot of what the previous recipe had proved: a probe that ran
        // while this request was open, or a TTL this request's evidence shortens below, cannot reach into it.
        let credited = {
            let agent = &inner.sessions[&key].agents[&agent_id];
            agent
                .current
                .as_ref()
                .and_then(|o| o.prior.as_ref())
                .and_then(|prior| {
                    let ttl = prior.ttl_ms.max(crate::config::FIVE_MINUTES_MS);
                    let gap_exceeded = agent.last_real_at.unwrap_or(now) - prior.real_at > ttl;
                    // The entry the probe kept has to be there when this request arrives, on the boundary the
                    // settlement and the scheduler read (`arrived < entry_expires`, `now >= expires`): a request
                    // at or after the lapse read a cache nothing the probe paid for kept, so that probe settles as
                    // expired, and a credit here would contradict that settlement.
                    let entry_alive = prior
                        .until
                        .is_some_and(|until| agent.last_real_at.unwrap_or(now) < until);
                    let same_model = prior.model == agent.canonical_model;
                    let fast = agent.current.as_ref().is_some_and(|o| o.recipe.fast);
                    // The upstream shares no cached prefix across speeds, so a probe on the other lane kept
                    // nothing this request read.
                    let same_speed = prior.fast == fast;
                    // The account that served this request, after any failover, has to be the one the
                    // covering probe warmed. A prefix cached elsewhere by ordinary traffic is no probe's to
                    // credit.
                    let same_account = prior.account == agent.account;
                    // A probe whose answer named no account (one the relay built before any selection)
                    // settled in no unit and funds nothing to credit, whatever `same_account` says of the
                    // account it was meant for.
                    let unit = prior.unit;
                    (prior.covered
                        && gap_exceeded
                        && entry_alive
                        && same_model
                        && same_speed
                        && same_account
                        && read > 0
                        && unit.is_some())
                    .then(|| {
                        let served = agent.current.as_ref();
                        (
                            read,
                            agent.key_generation,
                            agent.pool_id.clone(),
                            agent.account.clone(),
                            agent.canonical_model.clone(),
                            fast,
                            served.and_then(|o| o.kind),
                            served.is_some_and(|o| o.in_overage),
                        )
                    })
                })
        };
        inner.decide_unanswered(&key, &agent_id, read);
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
                // The three gates are pool, key, or credential properties that do not vary between attempts of
                // one request, so ending the observation must not recount a reason the request already carries.
                match current {
                    Some(
                        Reason::KeepaliveOff | Reason::OptedOut | Reason::IneligibleCredential,
                    ) if !ready => {
                        inner.end_observation(&key, &agent_id, None, Unanswered::Settle);
                    }
                    _ => inner.discard(&key, &agent_id, Reason::NoCacheEvidence, None),
                }
            }
        }
        if credited.is_some() {
            if let Some(agent) = inner
                .sessions
                .get_mut(&key)
                .and_then(|s| s.agents.get_mut(&agent_id))
            {
                agent.count_credit(now);
            }
        }
        drop(inner);
        if let Some((read, key_generation, pool_id, account, model, fast, kind, in_overage)) =
            credited
        {
            self.record_kept_warm(
                now,
                &key,
                key_generation,
                &pool_id,
                account.as_deref(),
                &model,
                &complete.request_id,
                read,
                fast,
                kind,
                in_overage,
            );
        }
    }
}

fn recipe_reason(reason: &str) -> Reason {
    match reason {
        "escaped_key" => Reason::InvalidRequest,
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
        let previous_ms = agent.ttl_ms;
        let requested_ms = observation
            .dispatched_ms
            .unwrap_or(observation.recipe.ttl_ms);
        agent.ttl_ms = usage.ttl_ms(previous_ms, requested_ms);
        if observation.ttl_ms > 0 && observation.ttl_ms < agent.ttl_ms {
            agent.ttl_ms = observation.ttl_ms;
        }
        note_ttl_change(
            key,
            agent_id,
            previous_ms,
            agent.ttl_ms,
            requested_ms,
            &usage,
        );
        observation.ttl_ms = agent.ttl_ms;
        agent.expires = Some(agent.cache_started + agent.ttl_ms);
        agent.jitter_frac = rand::random::<f64>();
        self.set_reason(key, agent_id, Reason::Scheduled);
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

impl Engine {
    /// One tick: what is due runs, up to the worker bound. Called by the production ticker every second and by
    /// the tests directly.
    pub async fn step(self: &Arc<Self>) {
        let now = self.now();
        let expiry = self.parts.session_expiry.as_millis() as i64;
        // The five asks behind `router_view` (canonicals, pins, cooldowns, overages, member spends) are a round
        // trip apiece, plus one `store.subscription` read per OAuth account; a tick with nothing due pays none of
        // them. `drop_sessions_of_dead_keys` reads the snapshot alone, which `RouterHandle::snapshot` answers
        // without a command.
        let (retained_models, swept) = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            inner.expire(now, expiry);
            let snapshot = self.parts.router.snapshot();
            inner.drop_sessions_of_dead_keys(&snapshot);
            inner.advance_idle_episodes(&snapshot, now);
            // A probe the ceiling wrote off keeps its entry alive for a turn that may still read it, so the write-off
            // waits for the entry's own end, whether or not any recipe is still retained.
            let swept = inner.settle_lapsed_pending(now);
            (
                inner.anything_retained().then(|| inner.retained_models()),
                swept,
            )
        };
        let Some(retained_models) = retained_models else {
            if swept {
                self.flush();
            }
            return;
        };
        let view = self.router_view(now, retained_models).await;
        // Built once for the whole tick, outside the lock, so every recipe's ceilings read the same prices and no
        // recipe rebuilds a table's rows.
        let rows = FactorRows::of(&self.parts.pricing.current(), &self.parts.tables);
        let (jobs, lapsed) = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            let mut lapsed: Vec<Skipped> = Vec::new();
            // A deleted account's failing streak goes with it, so a later account under the same id warns afresh.
            inner
                .unreadable_tiers
                .retain(|id| view.snapshot.account(id).is_some());
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
                // The ceilings and the gate run for every retained recipe, armed or not: a pause clears the
                // schedule, so an opt-out could otherwise never discard a recipe paused by a 429 and a ceiling
                // could never settle it while the companion keeps reporting.
                if agent.current.is_none() {
                    continue;
                }
                let ready = agent
                    .current
                    .as_ref()
                    .is_some_and(|o| o.ready && o.expected > 0);
                // Before any hold can short-circuit: a recipe held under `horizon` never reaches the gate, and the
                // ceilings below read the pool's policy as it stands now, so a horizon raised in the console
                // reopens the hold and an opt-out discards the prompt the hold retained.
                let (refreshed, moved) = {
                    let agent = inner
                        .sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(&agent_id)
                        .unwrap();
                    let before = (agent.policy.clone(), agent.pool_id.clone());
                    let refreshed = refresh_policy(&view, agent);
                    (
                        refreshed,
                        before != (agent.policy.clone(), agent.pool_id.clone()),
                    )
                };
                // Read at every tick, so only a change is written.
                if moved {
                    inner.touch_agent(&key, &agent_id);
                }
                if let Err(Gate::Drop(reason, label)) = refreshed {
                    inner.discard(&key, &agent_id, reason, Some(label));
                    continue;
                }
                // Ahead of the refresh's hold, so a deadline or a wake that passes while the pool is disabled is
                // recorded when it passes.
                inner.end_passed_park(&key, &agent_id, now);
                if let Err(Gate::Hold(reason)) = refreshed {
                    if inner.lapse(&key, &agent_id, now, &mut lapsed) {
                        continue;
                    }
                    // A paused recipe keeps its own reason, as under the ceilings' hold below: the pause already
                    // withholds every probe until the next real request.
                    if !inner.sessions[&key].agents[&agent_id].paused {
                        inner.hold(&key, &agent_id, reason, now);
                    }
                    continue;
                }
                // The refresh's own hold ends at the first tick that finds the pool enabled again, as the probe verdict
                // below ends `horizon`: nothing else rewrites it before the next probe, and until then an armed recipe
                // would read held with no next probe. A recipe whose request has not brought its evidence yet goes
                // back to waiting for it.
                if inner.sessions[&key].agents[&agent_id].reason == Reason::PoolDisabled {
                    let resumed = if ready {
                        Reason::Scheduled
                    } else {
                        Reason::AwaitingCacheEvidence
                    };
                    inner.set_reason(&key, &agent_id, resumed);
                }
                let verdict = {
                    let session = &inner.sessions[&key];
                    let agent = &session.agents[&agent_id];
                    let ceilings = ceilings_for(agent, &view, &rows, &self.parts.timing);
                    ceiling(
                        agent,
                        session.attended_at(now),
                        session.last_real_at,
                        &ceilings,
                        now,
                    )
                };
                match verdict {
                    Verdict::End(Reason::IdleLimit) => {
                        inner.end_observation(
                            &key,
                            &agent_id,
                            Some(Wasted::IdleLimit),
                            Unanswered::Keep,
                        );
                        inner.set_reason(&key, &agent_id, Reason::IdleLimit);
                        continue;
                    }
                    Verdict::End(reason) => {
                        inner.discard(&key, &agent_id, reason, wasted_label(reason));
                        continue;
                    }
                    Verdict::Hold(reason) => {
                        // A held recipe still ends at its entry's expiry: a cold body is worth nothing to the wake.
                        if inner.lapse(&key, &agent_id, now, &mut lapsed) {
                            continue;
                        }
                        let agent = &inner.sessions[&key].agents[&agent_id];
                        let (paused, began) = (agent.paused, agent.reason != reason);
                        // The probes paid so far kept nothing the wake will read, so they are written off as the
                        // hold begins. A paused recipe keeps its own reason and is written off on every held tick,
                        // which finds nothing left after the first.
                        if began || paused {
                            if let Some(label) = wasted_label(reason) {
                                inner.settle_unanswered(&key, &agent_id, label);
                            }
                        }
                        if !paused {
                            inner.hold(&key, &agent_id, reason, now);
                        }
                        continue;
                    }
                    Verdict::Probe => {
                        if inner.sessions[&key].agents[&agent_id].reason == Reason::Horizon {
                            inner.set_reason(&key, &agent_id, Reason::Scheduled);
                        }
                    }
                }
                if inner.lapse(&key, &agent_id, now, &mut lapsed) {
                    continue;
                }
                let (held, moved) = {
                    let agent = inner
                        .sessions
                        .get_mut(&key)
                        .unwrap()
                        .agents
                        .get_mut(&agent_id)
                        .unwrap();
                    let before = (
                        agent.policy.clone(),
                        agent.pool_id.clone(),
                        agent.account.clone(),
                    );
                    let held = gate(&view, agent, &key.1);
                    let after = (
                        agent.policy.clone(),
                        agent.pool_id.clone(),
                        agent.account.clone(),
                    );
                    (held, before != after)
                };
                if moved {
                    inner.touch_agent(&key, &agent_id);
                }
                match held {
                    Err(Gate::Drop(reason, label)) => {
                        inner.discard(&key, &agent_id, reason, Some(label));
                        continue;
                    }
                    Err(Gate::Hold(reason)) => {
                        inner.hold(&key, &agent_id, reason, now);
                        continue;
                    }
                    Ok(()) => {}
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
            due.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
            let mut jobs = Vec::new();
            for (_, key, agent_id) in due {
                if inner.running >= MAX_WORKERS {
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
                    counted_idle_tick: idle,
                };
                agent.in_flight = true;
                agent.hold_counted = false;
                agent.probes += 1;
                agent.count_probe(now);
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
            (jobs, lapsed)
        };
        self.flush();
        for skip in lapsed {
            self.record_skipped(now, &skip);
        }
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

    /// Quiesces, waits for the probes the quiesce cancelled, saves, and only then clears: the last save carries
    /// everything the engine held at the quiesce, and a flush already in flight finishes first under the flush lock.
    /// The caller drains the listeners after this, so a probe never outlives the relay it runs through.
    pub async fn shutdown(self: &Arc<Self>) {
        self.quiesce();
        self.workers.close();
        self.workers.wait().await;
        self.flush_persisted().await;
        self.close();
    }
}

pub(crate) struct RouterView {
    /// `(pool, account)` of every member whose spend rows refuse it now: the gate holds a probe pinned there.
    pub budgets: BTreeSet<(String, String)>,
    /// A retained model name to the one the router keys its rows on, for the names that differ.
    pub canonical: HashMap<String, String>,
    /// `(account, scope)` of every live cooldown row to its window, so the gate can tell a plan-window row apart.
    pub cooled: BTreeMap<(String, String), String>,
    pub overage: BTreeSet<String>,
    /// `(key, session, agent, canonical model)` to the pinned account: the account a probe would actually run
    /// on, so a sibling's pin never overwrites an agent's own.
    pub pins: BTreeMap<(String, String, String, String), String>,
    pub snapshot: Arc<RegistrySnapshot>,
}

/// What the gate decided for a due recipe: wait with a reason, or drop it with the label its probes settle under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Drop(Reason, Wasted),
    Hold(Reason),
}

impl Engine {
    /// Five asks of the router task per tick, whatever the number of recipes: the scheduler must not queue one
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
            .map(|p| ((p.key_id, p.session_id, p.agent_id, p.model), p.account_id))
            .collect();
        let cooled = self
            .parts
            .router
            .cooldowns()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.until > now)
            .map(|c| ((c.account_id, c.scope), c.window))
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
        let mut spends: BTreeMap<(String, String), Vec<MemberSpend>> = BTreeMap::new();
        for row in self.parts.router.member_spends().await.unwrap_or_default() {
            spends
                .entry((row.pool_id.clone(), row.account_id.clone()))
                .or_default()
                .push(row);
        }
        let budgets = spends
            .into_iter()
            .filter(|((pool_id, account_id), rows)| {
                let rows = rows.iter().collect::<Vec<_>>();
                snapshot
                    .members(pool_id)
                    .iter()
                    .find(|m| &m.account_id == account_id)
                    .is_some_and(|member| budget_deadline(member, &rows, now).is_some())
            })
            .map(|(member, _)| member)
            .collect();
        RouterView {
            budgets,
            canonical,
            cooled,
            overage,
            pins,
            snapshot,
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

/// The head of `gate` as one function both the gate and the step's ceilings call: the key's opt-out drops the
/// recipe, a disabled pool holds it, and the pool's current policy and id land on the agent, so a horizon raised in
/// the console reaches a held recipe at the next tick and an opt-out discards a held prompt rather than leaving it
/// retained.
pub(crate) fn refresh_policy(view: &RouterView, agent: &mut Agent) -> Result<(), Gate> {
    // The operator's opt-out reaches an idle session here, not only at its next request: the recipe is dropped
    // rather than held, because nothing on that key may be probed again until the flag is cleared.
    let Some(key) = view
        .snapshot
        .key(&agent.key_id)
        .filter(|k| !k.keepalive_opt_out)
    else {
        return Err(Gate::Drop(Reason::KeyOptedOut, Wasted::Released));
    };
    let Some(pool) = view.snapshot.pool(&key.pool_id).filter(|p| p.enabled) else {
        return Err(Gate::Hold(Reason::PoolDisabled));
    };
    // The pool's policy may have changed since admission, and an admin may have moved the key to another pool
    // without a request following. The ceilings, the budget and the probe's own delta all read the pool the key
    // belongs to now.
    agent.policy = KeepalivePolicy::from_json(&pool.keepalive_json);
    agent.pool_id = key.pool_id.clone();
    Ok(())
}

/// The ceilings of one agent at a tick: its policy, the factors of its model under its account's funding (dollars
/// at the fast cells for a fast recipe, dollars for an account the view lacks) from the tick's rows, its TTL and the
/// engine's timing. Tables that give no break-even (no priced row, or a free read) fall back to the shipped price
/// table's tightest row at the recipe's speed, so a ceiling is never unbounded. A timing whose lead is the whole TTL
/// leaves no probe interval, and nothing is probed.
pub(crate) fn ceilings_for(
    agent: &Agent,
    view: &RouterView,
    rows: &FactorRows,
    timing: &KeepaliveTiming,
) -> Ceilings {
    let fast = agent.current.as_ref().is_some_and(|o| o.recipe.fast);
    let funding = match agent
        .account
        .as_deref()
        .and_then(|id| view.snapshot.account(id))
    {
        Some(account) if !fast => Funding::of(account.kind, view.overage.contains(&account.id)),
        _ => Funding::ApiKey,
    };
    let ttl_ms = agent.ttl_ms.max(FIVE_MINUTES_MS);
    resolve(
        &agent.policy,
        rows.factors(
            funding,
            &agent.canonical_model,
            fast,
            agent.current.as_ref().map_or(0, |o| o.expected),
        ),
        ttl_ms,
        timing,
    )
    .or_else(|| resolve(&agent.policy, shipped_tightest(fast), ttl_ms, timing))
    .unwrap_or(Ceilings {
        idle_ms: 0,
        parked_ms: 0,
        horizon_ms: 0,
        break_even_ms: 0,
        idle_share: 0.0,
    })
}

/// The spec's "Scheduling against router state" row, in its order. `Err` names why the due probe waits, or why
/// the recipe goes; `Ok` lets the probe run.
pub(crate) fn gate(view: &RouterView, agent: &mut Agent, session_id: &str) -> Result<(), Gate> {
    refresh_policy(view, agent)?;
    let key_id = agent.key_id.clone();
    let Some(pool) = view.snapshot.pool(&agent.pool_id) else {
        return Err(Gate::Hold(Reason::PoolDisabled));
    };
    // A probe keeps the session on the account it warmed only while the pin outlives the probe's due time:
    // the recipe's own TTL less the lead, so a five-minute recipe needs about four minutes of pin and a
    // one-hour one about fifty-four. The save-time floor of 300 s stays the one hard refusal.
    let due_after_ms = agent.ttl_ms.max(FIVE_MINUTES_MS) - agent.timing.lead_ms(agent.ttl_ms);
    if (pool.affinity_ttl_seconds as i64).saturating_mul(1_000) < due_after_ms {
        return Err(Gate::Hold(Reason::AffinityTooShort));
    }
    if !agent.policy.enabled {
        return Err(Gate::Hold(Reason::KeepaliveOff));
    }
    // The pin is what the probe will follow, so the pin's account is the one to gate: the router may have moved
    // the agent's own pin between this recipe's last observation and the probe.
    let pinned = view.pins.get(&(
        key_id.clone(),
        session_id.to_string(),
        agent.id.clone(),
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
        return Err(Gate::Hold(Reason::Paused));
    };
    if !account.enabled {
        return Err(Gate::Hold(Reason::Paused));
    }
    if matches!(
        account.state,
        AccountState::ReauthRequired | AccountState::FundingRequired
    ) {
        return Err(Gate::Hold(Reason::Reauth));
    }
    let fast = agent
        .current
        .as_ref()
        .map(|o| &o.recipe)
        .is_some_and(|r| r.fast);
    // The router's own rule decides which live row holds the probe: a fast recipe passes a plan-window row on the
    // account and holds on its own lane's, as a fast request does.
    let cooled = view.cooled.iter().any(|((id, scope), window)| {
        id == &account.id
            && phoebis_routing::cooldown_holds(scope, window, &agent.canonical_model, fast)
    });
    if cooled {
        return Err(Gate::Hold(Reason::Cooled));
    }
    // A fast recipe draws usage credits, not a plan window, so plan overage does not hold it.
    if !fast && view.overage.contains(&account.id) && pool.overage_policy != OveragePolicy::Allow {
        return Err(Gate::Hold(Reason::Overage));
    }
    // Read live on every poll like the two holds above: the period's end or a raised budget clears it on the
    // next tick, and a real request that moves the pin clears it through the pin.
    if view
        .budgets
        .contains(&(pool.id.clone(), account.id.clone()))
    {
        return Err(Gate::Hold(Reason::Budget));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is kept by hand. The cycle below names every variant with no wildcard, so a variant the enum gains stops
    /// this test compiling until it joins the cycle, and the walk then wants it in `ALL`.
    #[test]
    fn wasted_all_holds_every_variant() {
        let next = |label: Wasted| match label {
            Wasted::ColdReturn => Wasted::Evicted,
            Wasted::Evicted => Wasted::Expired,
            Wasted::Expired => Wasted::Horizon,
            Wasted::Horizon => Wasted::IdleLimit,
            Wasted::IdleLimit => Wasted::ParkedLimit,
            Wasted::ParkedLimit => Wasted::Released,
            Wasted::Released => Wasted::Replaced,
            Wasted::Replaced => Wasted::SessionEnded,
            Wasted::SessionEnded => Wasted::ColdReturn,
        };
        let mut walked = vec![Wasted::ColdReturn];
        let mut label = next(Wasted::ColdReturn);
        while label != Wasted::ColdReturn && walked.len() <= Wasted::ALL.len() {
            walked.push(label);
            label = next(label);
        }
        assert_eq!(walked, Wasted::ALL.to_vec());
    }

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

    /// One pool holding one key and two members, subscription account `o` and api_key account `a`, with keepalive
    /// on.
    fn pool_view(o_in_overage: bool) -> RouterView {
        pool_view_under(o_in_overage, OveragePolicy::Allow)
    }

    /// `pool_view` with the pool's overage policy named, for a case the overage hold has to reach.
    fn pool_view_under(o_in_overage: bool, overage_policy: OveragePolicy) -> RouterView {
        let policy = KeepalivePolicy {
            enabled: true,
            ..KeepalivePolicy::default()
        };
        let account = |id: &str, kind| phoebis_store::AccountRow {
            id: id.into(),
            kind,
            label: id.into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: phoebis_store::StateCause::Operator,
        };
        let member = |account_id: &str| phoebis_store::PoolMember {
            pool_id: "main".into(),
            account_id: account_id.into(),
            priority: 0,
            budget_daily_usd: None,
            budget_monthly_usd: None,
        };
        let load = phoebis_store::RegistryLoad {
            accounts: vec![
                account("a", AccountKind::ApiKey),
                account("o", AccountKind::Oauth),
            ],
            keys: vec![phoebis_store::ApiKeyRow {
                id: "k".into(),
                label: "k".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: Vec::new(),
                model_rules_mode: phoebis_store::RulesMode::default(),
                long_context_twins: false,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: vec![0; 32],
                prefix: "00000000".into(),
                incarnation: 1,
                created_at: 0,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
            }],
            members: vec![member("a"), member("o")],
            pools: vec![phoebis_store::PoolRow {
                overage_policy,
                keepalive_json: serde_json::to_string(&policy).unwrap(),
                prefer_subscription: false,
                ..phoebis_store::PoolRow::for_tests("main")
            }],
            ..phoebis_store::RegistryLoad::default()
        };
        RouterView {
            budgets: BTreeSet::new(),
            canonical: HashMap::new(),
            cooled: BTreeMap::new(),
            overage: if o_in_overage {
                BTreeSet::from(["o".to_string()])
            } else {
                BTreeSet::new()
            },
            pins: BTreeMap::new(),
            snapshot: Arc::new(RegistrySnapshot::from_load(load, [0; 32], 0, 1)),
        }
    }

    /// An observation whose recipe `make_recipe` built from a body sent in fast mode, or at standard speed.
    fn observation(fast: bool) -> Observation {
        let speed = if fast { r#""speed":"fast","# } else { "" };
        let body = format!(
            r#"{{"model":"claude-opus-5-5",{speed}"max_tokens":1024,"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let recipe = make_recipe(&[], body.as_bytes(), "claude-opus-5-5", 1 << 20).unwrap();
        assert_eq!(recipe.fast, fast, "{body}");
        Observation {
            id: "r".into(),
            attempt: 1,
            started: 0,
            recipe,
            ready: true,
            expected: 2_000,
            ttl_ms: 0,
            prior: None,
            poisoned: false,
            dispatched_ms: None,
            kind: None,
            in_overage: false,
        }
    }

    /// The gate's answer for `main` pinned on `account`, holding a recipe of that speed.
    fn gated_at(view: &RouterView, account: &str, fast: bool) -> Option<Gate> {
        let mut agent = Agent::new("main", "k", 1, 0, KeepaliveTiming::default());
        agent.account = Some(account.into());
        agent.canonical_model = "claude-opus-5-5".into();
        agent.current = Some(observation(fast));
        gate(view, &mut agent, "s").err()
    }

    /// A recipe's ceilings read the price set its prompt selects, its length the tokens the cache evidence counted: the
    /// fixture's body is under a hundred bytes, and its 2,000 tokens put it over the threshold.
    #[test]
    fn a_recipes_ceilings_read_the_price_set_its_prompt_tokens_select() {
        let pricing = phoebis_ledger::PriceTable::from_yaml(
            r#"
effective: 2026-01-01
models:
  claude-opus-5-5:
    input: 4
    cache_write_5m: 1.25
    cache_write_1h: 2
    cache_read: 0.1
    output: 20
    long_context: {over: 1000, input: 8, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.5, output: 40}
"#,
            "test",
        )
        .unwrap();
        let rows = FactorRows::of(&pricing, &PlanTables::shipped());
        let timing = KeepaliveTiming::default();
        let mut agent = Agent::new("main", "k", 1, 0, timing);
        agent.account = Some("a".into());
        agent.canonical_model = "claude-opus-5-5".into();
        agent.current = Some(observation(false));
        let break_even =
            |agent: &Agent| ceilings_for(agent, &pool_view(false), &rows, &timing).break_even_ms;
        assert_eq!(
            break_even(&agent),
            960_000,
            "over the threshold the long-context cells' 0.5 against 2.5 break even after 16 minutes"
        );
        agent.current.as_mut().unwrap().expected = 1_000;
        assert_eq!(
            break_even(&agent),
            2_760_000,
            "at the threshold the standard cells' 0.1 against 1.25 break even after 46 minutes"
        );
    }

    #[test]
    fn a_fast_recipe_passes_a_plan_window_hold_takes_no_overage_hold_and_holds_on_a_fast_row() {
        let cooled = |scope: &str, window: &str| RouterView {
            cooled: BTreeMap::from([(("o".to_string(), scope.to_string()), window.to_string())]),
            ..pool_view(false)
        };
        let plan_window = cooled("account", "five_hour");
        assert_eq!(
            (
                gated_at(&plan_window, "o", true),
                gated_at(&plan_window, "o", false)
            ),
            (None, Some(Gate::Hold(Reason::Cooled))),
            "an account row on a plan window holds the standard recipe alone"
        );
        let fast_row = cooled("fast", "fast");
        assert_eq!(
            (
                gated_at(&fast_row, "o", true),
                gated_at(&fast_row, "o", false)
            ),
            (Some(Gate::Hold(Reason::Cooled)), None),
            "the fast lane's row holds the fast recipe alone"
        );
        let overage = pool_view_under(true, OveragePolicy::Never);
        assert_eq!(
            (
                gated_at(&overage, "o", true),
                gated_at(&overage, "o", false)
            ),
            (None, Some(Gate::Hold(Reason::Overage))),
            "plan overage holds the standard recipe alone"
        );
    }

    #[test]
    fn the_gate_holds_on_a_cooldown_row_exactly_when_the_routers_rule_does() {
        let rows = [
            ("account", "five_hour"),
            ("account", "seven_day"),
            ("account", "seven_day_fable"),
            ("account", "retry_after"),
            ("fast", "fast"),
            ("model:claude-opus-5-5", "retry_after"),
            ("model:claude-sonnet-5", "retry_after"),
        ];
        let mut answers = BTreeSet::new();
        for (scope, window) in rows {
            for fast in [false, true] {
                let view = RouterView {
                    cooled: BTreeMap::from([(
                        ("o".to_string(), scope.to_string()),
                        window.to_string(),
                    )]),
                    ..pool_view(false)
                };
                let holds = phoebis_routing::cooldown_holds(scope, window, "claude-opus-5-5", fast);
                assert_eq!(
                    gated_at(&view, "o", fast) == Some(Gate::Hold(Reason::Cooled)),
                    holds,
                    "{scope} at {window}, fast {fast}"
                );
                answers.insert(holds);
            }
        }
        assert_eq!(answers.len(), 2, "the table reaches both answers");
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
