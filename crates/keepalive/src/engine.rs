//! The engine: every session the companion registered or a request revealed, every agent's recipe, and the
//! counters behind the views. One mutex, as the Go engine had, held for microseconds at a time. Nothing under
//! it does I/O.

use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{
    spawn_subscriber, user_agent_of, AccountSelected, Bus, Delivery, Event as BusEvent, Outcome,
    Phase, RequestComplete, RequestObserved, ResponseChunk, ResponseHead,
};
use phoebis_ledger::{funding_of, Band, Ledger, Metrics, PlanTables, Pricing, Speed};
use phoebis_oauth::Clock;
use phoebis_relay::{CacheTtl, CacheTtlPolicy, Relay};
use phoebis_routing::{
    budget_deadline, listing, seconds_ms, Holder, MemberSpend, PinUsage, RegistrySnapshot,
    RouterHandle, DAY_MS, SEVEN_DAY,
};
use phoebis_store::{AccountKind, AccountState, CooldownScope, OveragePolicy, PoolRow, Store};
#[cfg(any(test, feature = "debug-hooks"))]
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::breakeven::{resolve_or_shipped, CeilingInputs, Ceilings, FactorRows};
use crate::config::{
    floor_ttl_ms, KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, FIVE_MINUTES_MS, MAX_WORKERS,
    ONE_HOUR_MS,
};
use crate::facts::{AgentFacts, SessionFacts};
use crate::persist::Dirty;
use crate::probe::{ceiling, run_job, wasted_label, ProbeJob};
use crate::recipe::{make_recipe, Recipe};
use crate::savings::{AttemptFunding, RowClient};
use crate::usage::ProbeUsage;
use crate::view::{recipe_state, RecipeState};

const LOCK_POISONED: &str = "keepalive engine poisoned by a prior panic";

/// The main conversation's agent, which the idle limits apply to.
pub use phoebis_routing::MAIN_AGENT;

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

    pub(crate) fn background(self) -> bool {
        matches!(self, ParkKind::Background | ParkKind::Both)
    }

    pub(crate) fn scheduled(self) -> bool {
        matches!(self, ParkKind::Scheduled | ParkKind::Both)
    }

    fn of_halves(background: bool, scheduled: bool) -> Option<ParkKind> {
        match (background, scheduled) {
            (true, true) => Some(ParkKind::Both),
            (true, false) => Some(ParkKind::Background),
            (false, true) => Some(ParkKind::Scheduled),
            (false, false) => None,
        }
    }
}

/// What a parked agent waits on and until when, held as one value so that a park with no kind cannot be represented
/// and no reader has to compensate for a deadline or a wake that a pass left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Park {
    #[serde(skip_serializing_if = "Option::is_none")]
    deadline: Option<i64>,
    #[serde(rename = "park_kind")]
    kind: ParkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    wake_at: Option<i64>,
}

impl Park {
    /// The park of `kind`, keeping the deadline only on a background kind and the wake only on a scheduled one, so no
    /// park holds a part its kind does not take.
    pub(crate) fn new(kind: ParkKind, deadline: Option<i64>, wake_at: Option<i64>) -> Park {
        Park {
            deadline: deadline.filter(|_| kind.background()),
            kind,
            wake_at: wake_at.filter(|_| kind.scheduled()),
        }
    }

    pub(crate) fn kind(self) -> ParkKind {
        self.kind
    }

    pub(crate) fn deadline(self) -> Option<i64> {
        self.deadline
    }

    pub(crate) fn wake_at(self) -> Option<i64> {
        self.wake_at
    }

    /// The park left at `now` once each part whose return came without a request drops out with its half: a deadline
    /// the harness stopped the command at, or a wake that passed. `None` when no half is left, which is when it ended.
    pub(crate) fn passed(self, now: i64) -> Option<Park> {
        let deadline_passed = self.deadline.is_some_and(|deadline| deadline <= now);
        let wake_passed = self.wake_at.is_some_and(|wake| wake <= now);
        ParkKind::of_halves(
            self.kind.background() && !deadline_passed,
            self.kind.scheduled() && !wake_passed,
        )
        .map(|kind| {
            Park::new(
                kind,
                self.deadline.filter(|_| !deadline_passed),
                self.wake_at.filter(|_| !wake_passed),
            )
        })
    }

    /// For the readers whose view or saved row keeps the deadline, the kind and the wake as members of their own.
    pub(crate) fn parts(park: Option<Park>) -> (Option<i64>, Option<ParkKind>, Option<i64>) {
        park.map_or((None, None, None), |park| {
            (park.deadline, Some(park.kind), park.wake_at)
        })
    }
}

/// A park as the engine holds it: the reported value with the start the parked ceiling counts from, which no report
/// or answer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeldPark {
    pub began: i64,
    pub park: Park,
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
    AgentLimit => "agent_limit",
    AgentReleased => "agent_released",
    AgentUnknown => "agent_unknown",
    AwaitingCacheEvidence => "awaiting_cache_evidence",
    AwaitingRequest => "awaiting_request",
    AwaitingSelection => "awaiting_selection",
    Budget => "budget",
    BusGap => "bus_gap",
    Compacted => "compacted",
    Cooled => "cooled",
    Credits => "credits",
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
    NoPin => "no_pin",
    NoProbeCacheEvidence => "no_probe_cache_evidence",
    OptedOut => "opted_out",
    Overage => "overage",
    OwnerMismatch => "owner_mismatch",
    ParkedLimit => "parked_limit",
    PartialCacheCoverage => "partial_cache_coverage",
    Paused => "paused",
    PinMoved => "pin_moved",
    PoolDisabled => "pool_disabled",
    ProbeAuthenticationError => "probe_authentication_error",
    ProbePoolExhausted => "probe_pool_exhausted",
    ProbeQuotaExhausted => "probe_quota_exhausted",
    ProbeRetryLimit => "probe_retry_limit",
    ProbeTransportError => "probe_transport_error",
    Probing => "probing",
    Reauth => "reauth",
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
    SessionLimit => "session_limit",
    SlowProbe => "slow_probe",
    Spent => "spent",
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

/// A stored word reads back as its cause, and a word this build does not know refuses the payload.
impl<'de> serde::Deserialize<'de> for Reason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Reason, D::Error> {
        let word = String::deserialize(deserializer)?;
        Reason::ALL
            .iter()
            .copied()
            .find(|reason| reason.as_str() == word)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown reason {word}")))
    }
}

/// The share of the previous main prompt a completion must read from the cache, under a chain that holds none of the
/// anchor's boundaries, for the engine to read a contradiction between its chain model and the upstream's.
pub const CHAIN_CONTRADICTION_COVERAGE: f64 = 0.9;

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
    Compaction,
    Foreign,
    Rearm,
    Restart,
}

/// The guard of a request that names its class: one answer for `guard`, the chain mark and the prebuild, so a class
/// added to `REFRESHING_CLASSES` reaches all three.
fn class_guard(class: &str) -> Guard {
    // A compaction reads the old prefix to write a context the next turn starts from, so a recipe armed from it
    // would keep warm a body no turn reads again.
    if class == "compaction" {
        Guard::Compaction
    } else if phoebis_routing::REFRESHING_CLASSES.contains(&class) {
        Guard::Rearm
    } else {
        Guard::Auxiliary
    }
}

/// The hijack guard: a class names the turn directly. Without one, the request's own chain against the armed
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
        return class_guard(class);
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
    pub anchor: Option<PrefixAnchor>,
    pub prompt_at: Option<i64>,
    pub pool_key: String,
    /// The UTC day the episode's highest counted bucket was reached, where its return counts, so a day never
    /// answers a return without its reach. `None` until a bucket is counted, and on a window restored from a row saved
    /// without it, whose reach is held under no day, so its return counts nothing unless the request raises a bucket.
    pub bucket_day: Option<i64>,
}

/// The prefix an idle window judges a return against: the request's static hash, its last message boundary, the one
/// before it (`None` for a one-message chain) and the deepest marked one before those two. A return holds any of the
/// three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixAnchor {
    pub static_hash: String,
    pub last: String,
    pub before_last: Option<String>,
    /// The deepest boundary before the last two that the request marked with `cache_control`, which the upstream
    /// reads the cache through.
    pub marked: Option<String>,
}

/// Whether a request continues the prefix `anchor` names: one of its message boundaries equals the anchor's last
/// boundary, the one before it or its marked one, so a return counts when the client re-sent the last message in a
/// shape the canonical form does not restore and the message before it held, and when an interrupted turn's next
/// request dropped both and kept the marked boundary the upstream reads the cache through. The chain is the only
/// evidence, and the request's class is none: a refreshing class whose chain holds none of them answers false where
/// `guard` re-arms it, as do a request with no chain and an absent anchor.
pub fn continues_prefix(anchor: Option<&PrefixAnchor>, boundaries: &[String]) -> bool {
    anchor.is_some_and(|anchor| {
        boundaries.iter().any(|boundary| {
            *boundary == anchor.last
                || anchor.before_last.as_ref() == Some(boundary)
                || anchor.marked.as_ref() == Some(boundary)
        })
    })
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
        .key_pool(&agent.key_id)
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
        bucket_day: None,
    });
    true
}

/// The routing crate's unit selector, so the keepalive and the shedding round never disagree on a pin's unit.
pub type SavingsUnit = phoebis_routing::BurnUnit;

/// How the attempt that served a probe was paid for, the ledger's own reading of an attempt's funding, so the
/// break-even factors and the rows a probe files under follow one rule. The keepalive reads it at standard speed, since
/// a fast recipe reads the fast cells whatever funds it.
pub use phoebis_ledger::Funding;

/// The fundings a pool's members bring to its ceilings, from each member's kind and usage-based class with no
/// overage, each once in the members' order: an OAuth member brings the plan's factors, an API-key member the dollar
/// rows, and a usage-based seat the dollar rows too.
pub fn pool_fundings(members: impl IntoIterator<Item = (AccountKind, bool)>) -> Vec<Funding> {
    let mut fundings: Vec<Funding> = Vec::new();
    for (kind, usage_based) in members {
        let funding = funding_of(Some(kind), usage_based, Some(false), Speed::Standard);
        if !fundings.contains(&funding) {
            fundings.push(funding);
        }
    }
    fundings
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
    /// The registry's own store, read for the sessions and recipes a restart resumes and for the key the companion's
    /// account pseudonyms are digested under.
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
    /// The last bus gap's instant, `None` before the first.
    pub last_bus_gap_at: Option<i64>,
    /// The request events (a selection, a head, a chunk, a completion, a new request) and the reports refused because
    /// they arrived after the shutdown's quiesce, so the state the last save holds is the state at the quiesce.
    pub late_updates: u64,
    /// The sessions `max_sessions` bounds: a registered session that has sent no real request holds no slot, and an
    /// ended session keeps its entry until the expiry and holds none.
    pub live_sessions: usize,
    pub retained_bytes: usize,
    pub running_probes: usize,
    /// Every session entry the engine holds: a session that holds no slot counts here and not in `live_sessions`.
    pub sessions: usize,
}

/// One companion registration, or one session a request revealed before any registration.
pub(crate) struct Session {
    /// The generation of the key that registered the session, so a key id created again reaches none of the
    /// previous holder's sessions: the companion identity is the pair, never the id alone.
    pub key_generation: i64,
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
    /// The session's last real request, one refused its slot included, which the expiry and a silent session's idle
    /// ceiling count from. A refused session keeps no recipe, so the ceiling never reads a refused request's stamp.
    pub last_real_at: Option<i64>,
    /// Whether the session took its slots of `max_sessions` and `max_agents`, which the first request the caps admit
    /// sets. From then on its agents' records hold slots of `max_agents` until each leaves, a record its end released
    /// included. A request refused the slot leaves it false and still writes `last_real_at`, so the session's next
    /// request reads both caps again while the expiry counts from the refused request.
    pub slotted: bool,
    /// A session the load restored from a save that held it attended keeps the attended reading of the ceilings until
    /// this instant or until the mod registers it again, whichever is first: the mod re-adopts a restored session
    /// within its lease, and a parked recipe must not be discarded as idle before it can.
    pub restored_until: Option<i64>,
    /// The requests that named another owner than the registration while the holder's lease was live, so the
    /// warning goes out once per session rather than once per request.
    pub owner_mismatches: u64,
    /// What the companion reports of the session for the Sessions view. Display state that no decision of the engine
    /// reads.
    pub facts: SessionFacts,
    pub agents: BTreeMap<String, Agent>,
}

impl Session {
    /// Whether the ceilings read the session as attended at `now`: as the mod's lease says, or as the save said for
    /// one lease length after a load.
    pub(crate) fn attended_at(&self, now: i64) -> bool {
        self.attended || self.restored_until.is_some_and(|until| now < until)
    }

    /// Holds a slot of `max_sessions`: it took its slots and has not ended.
    pub(crate) fn holds_slot(&self) -> bool {
        !self.ended && self.slotted
    }

    /// The slots of `max_agents` the session's agents hold, none before the session takes its slots.
    pub(crate) fn agent_slots(&self) -> usize {
        if self.slotted {
            self.agents.len()
        } else {
            0
        }
    }
}

pub(crate) struct Agent {
    pub id: String,
    /// The session's key, repeated here so a gate over one agent needs no session lookup.
    pub key_id: String,
    /// The generation of the key the recipe was retained under, off the observed request. A probe's and a
    /// settlement's rows file under it.
    pub key_generation: i64,
    /// Which record this is. A replacement registration builds a new record under the same ids, and a probe
    /// worker that started on the old one must not touch the new one's bookkeeping.
    pub incarnation: u64,
    /// Written through `set_state`, which ends the park on any move out of `Parked`.
    pub state: AgentState,
    /// The park the last `parked` report described, cleared on every move out of `Parked`, so a reader never finds a
    /// park on an agent that is not parked. Its start is kept across a repeated `parked` of the same kind.
    pub park: Option<HeldPark>,
    /// When a deadline or a wake passed without a request: the idle window restarts here. No report clears it,
    /// since the window starts at the later of it and `last_activity` and an older end never outranks a newer
    /// request.
    pub park_ended_at: Option<i64>,
    /// The open idle window of main, `None` while none is counted.
    pub idle_episode: Option<IdleEpisode>,
    /// The start of main's last closed idle window. The next opens only from a start past it, so a close that leaves
    /// the start in place, a foreign request or a lapse a registration adopts again, never reopens the same window.
    /// An event that moves the next window's start on purpose (an `agent_start` of a released main, a park's end, a
    /// presence report, a request, a revival) clears it: the floor orders starts, and a clock that stepped back before
    /// that event is no ordering.
    pub idle_window_floor: Option<i64>,
    /// The anchor of main's last admitted request, retained or not, which the next idle window judges a return against
    /// and the chain watch reads the next request's chain against.
    pub last_prefix: Option<PrefixAnchor>,
    /// The previous main request's prompt as its completion reported it: uncached input, cache reads and both
    /// cache writes, set by every pending main completion and every marked one, so a refused recipe's unmarked
    /// request leaves it as it was. What the watch reads the next completion's cache reads against.
    pub last_prompt_tokens: Option<u64>,
    pub generation: u64,
    pub current: Option<Observation>,
    /// Never saved: a restart keeps the current observation alone.
    pub aside: Option<AsideRecipe>,
    /// The compaction open on the agent, held apart from `current` so none of its events moves the recipe. Never saved:
    /// a restart drops it with the events that would have completed it.
    pub compaction: Option<OpenCompaction>,
    /// The recipe was set or replaced since the last landed save that carried its body, so the next save seals it.
    pub body_dirty: bool,
    /// Whether the registry may hold a sealed body under this agent's row, so a save without a recipe clears it. A new
    /// record starts true, since it can take over a row an earlier record sealed a body into, and an acknowledged seal
    /// or clear sets it to what the registry then holds.
    pub body_saved: bool,
    /// When the entry the recipe keeps warm expires. The due time is this less the timing's lead and jitter.
    pub expires: Option<i64>,
    /// When a released record leaves the engine: the release plus the longest TTL of the pool's cache TTL policy,
    /// `None` on a record that is not released.
    pub leaves_at: Option<i64>,
    /// The share of the timing's jitter this schedule takes, rolled once per schedule so the due time holds
    /// still between ticks.
    pub jitter_frac: f64,
    /// An outstanding backoff after a failed probe. The due time is the later of it and the expiry's.
    pub retry_at: Option<i64>,
    /// The engine's timing, held here so a due time and a retry never need the engine.
    pub timing: KeepaliveTiming,
    pub last_activity: i64,
    pub cache_started: i64,
    pub ttl_ms: i64,
    pub cold: u32,
    pub failures: u32,
    pub account: Option<String>,
    /// The incarnation of `account` when the armed recipe was last answered on it. `None` on a record saved before
    /// agents carried one, which holds under `pin_moved` until the agent's next request arms the recipe.
    pub account_incarnation: Option<u64>,
    pub pool_id: String,
    /// The canonical model of the request the recipe came from, as the relay resolved it: the price lookups,
    /// the rollup keys, the cooldown scope, and the views read this, never the caller's spelling in the recipe.
    pub canonical_model: String,
    /// The entrypoint of the observed request's `client` label, what its probes price under, and `None` for a label
    /// that names none.
    pub entrypoint: Option<String>,
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
    pub usage: ProbeUsage,
    /// Every probe since the last real request with the entry it wrote, settled as wasted when the recipe stops, and
    /// the newest covered one, else the newest, the coverage the next request reads.
    pub unanswered: Vec<UnansweredProbe>,
    /// The probes a superseding real request took over from `unanswered`, with the keys they were paid under: the
    /// request moves the agent's model, account, pool and key generation before its completion decides them. A
    /// probe sent while it is open lands on `unanswered`, for the request after it. The idle limit moves its probes
    /// here too, pending under its label until their entry lapses or a turn reads them, and a failed turn hands back
    /// the probes it took the same way.
    pub kept: Vec<Kept>,
    pub last_real_at: Option<i64>,
    /// The arrival of the last request whose completion succeeded with a 2xx status, or of a turn whose completion the
    /// engine missed, which the next arrival or a bus gap ends with none observed. The kept-warm gap is measured from
    /// it rather than from `last_real_at`, so a failed turn, which stays no evidence about the cache, leaves the retry
    /// after it the gap from the last turn that answered.
    pub last_answered_at: Option<i64>,
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
    /// What the companion reports of the agent for the Sessions view. Display state that no decision of the engine
    /// reads.
    pub facts: AgentFacts,
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
            park: None,
            park_ended_at: None,
            idle_episode: None,
            idle_window_floor: None,
            last_prefix: None,
            last_prompt_tokens: None,
            generation: 0,
            current: None,
            aside: None,
            compaction: None,
            body_dirty: false,
            body_saved: true,
            expires: None,
            leaves_at: None,
            jitter_frac: 0.0,
            retry_at: None,
            timing,
            last_activity: now,
            cache_started: now,
            ttl_ms: 0,
            cold: 0,
            failures: 0,
            account: None,
            account_incarnation: None,
            pool_id: String::new(),
            canonical_model: String::new(),
            entrypoint: None,
            reason: Reason::AwaitingRequest,
            last_guard: None,
            hold_counted: false,
            paused: false,
            in_flight: false,
            probes: 0,
            usage: ProbeUsage::default(),
            unanswered: Vec::new(),
            kept: Vec::new(),
            last_real_at: None,
            last_answered_at: None,
            last_stop: None,
            last_presence_at: None,
            last_probe_at: None,
            probes_today: 0,
            credited_today: 0,
            counters_day: utc_day(now),
            facts: AgentFacts::default(),
        }
    }

    /// Moves every model name the agent holds that `canonical` dates, and answers whether any moved: the recipe's
    /// own, the one its open observation took from the recipe before it, an open compaction's own and the one it took,
    /// and its kept and unanswered probes', which an agent keeps after its recipe ended.
    pub(crate) fn follow_resolutions(&mut self, canonical: &HashMap<String, String>) -> bool {
        let follow = |model: &mut String| match canonical.get(model.as_str()) {
            Some(dated) => {
                *model = dated.clone();
                true
            }
            None => false,
        };
        let mut moved = follow(&mut self.canonical_model);
        if let Some(prior) = self.current.as_mut().and_then(|o| o.prior.as_mut()) {
            moved |= follow(&mut prior.model);
        }
        // The completion credits through the compaction's `prior` and settles the probes against its own model, so
        // both follow the probes' move, or a credit would sit beside a cold return of the probe that earned it.
        if let Some(compaction) = self.compaction.as_mut() {
            moved |= follow(&mut compaction.canonical_model);
            if let Some(prior) = compaction.prior.as_mut() {
                moved |= follow(&mut prior.model);
            }
        }
        for kept in &mut self.kept {
            moved |= follow(&mut kept.canonical_model);
        }
        for probe in &mut self.unanswered {
            moved |= follow(&mut probe.model);
        }
        moved
    }

    /// Every probe the agent paid for that no request answered, the unanswered ones and then the kept ones, settled
    /// under `label`.
    pub(crate) fn settle_all(&mut self, key: &SessionKey, label: Wasted) -> Vec<Settlement> {
        self.take_unanswered()
            .into_iter()
            .chain(std::mem::take(&mut self.kept))
            .map(|kept| kept.settle(key, label))
            .collect()
    }

    /// The unanswered probes, taken off the agent as kept ones.
    pub(crate) fn take_unanswered(&mut self) -> Vec<Kept> {
        let unanswered = std::mem::take(&mut self.unanswered);
        unanswered
            .into_iter()
            .map(|probe| Kept::of(self, probe))
            .collect()
    }

    /// The names `follow_resolutions` reads on an agent with no compaction open, as an arrival leaves it once
    /// `end_compaction` ran: the ones the arrival looks up in the catalog's acknowledged resolution.
    pub(crate) fn held_models(&self) -> BTreeSet<String> {
        let mut models = BTreeSet::from([self.canonical_model.clone()]);
        models.extend(
            self.current
                .as_ref()
                .and_then(|o| o.prior.as_ref())
                .map(|prior| prior.model.clone()),
        );
        models.extend(self.kept.iter().map(|kept| kept.canonical_model.clone()));
        models.extend(self.unanswered.iter().map(|probe| probe.model.clone()));
        models
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
        from_expiry.max(self.retry_at)
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
            self.park = None;
        }
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
struct Answer {
    started: Option<i64>,
    account: Option<String>,
    model: String,
    fast: bool,
    read: u64,
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

/// What a walk whose caller just read the agent under the same lock expects of it.
pub(crate) const AGENT_HELD: &str =
    "an agent the caller read under the lock is still in its session";

/// The agent `agent_id` of the session `key`, borrowing the session map alone, so a caller may write another field of
/// `Inner` while it holds the agent.
pub(crate) fn agent_mut<'a>(
    sessions: &'a mut BTreeMap<SessionKey, Session>,
    key: &SessionKey,
    agent_id: &str,
) -> Option<&'a mut Agent> {
    sessions.get_mut(key)?.agents.get_mut(agent_id)
}

/// `agent_mut`'s shared twin.
pub(crate) fn agent_ref<'a>(
    sessions: &'a BTreeMap<SessionKey, Session>,
    key: &SessionKey,
    agent_id: &str,
) -> Option<&'a Agent> {
    sessions.get(key)?.agents.get(agent_id)
}

/// Drops the agent's observation, with the pending entry it follows and the bytes its recipe held.
fn drop_current(
    agent: &mut Agent,
    pending: &mut HashMap<String, (SessionKey, String)>,
    retained: &mut usize,
) {
    if let Some(observation) = agent.current.take() {
        pending.remove(&observation.id);
        *retained -= observation.recipe.size;
    }
}

/// Whether a request the engine follows is open on `agent`: the turn its observation follows, while `pending` holds
/// it, or a compaction. Its completion decides the probes and the recipe, so no lapse, eviction or sweep decides them
/// first.
fn request_open(pending: &HashMap<String, (SessionKey, String)>, agent: &Agent) -> bool {
    agent.compaction.is_some()
        || agent
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
fn prior_coverage(agent: &Agent, now: i64) -> Option<PriorCoverage> {
    let newest_kept = agent
        .kept
        .iter()
        .rev()
        .find(|kept| kept.pending.is_some() && now < kept.entry_expires);
    let newest_unanswered = agent
        .unanswered
        .iter()
        .rev()
        .find(|p| p.covered)
        .or(agent.unanswered.last());
    let answered_at = agent.last_answered_at.or(agent.last_real_at);
    answered_at.map(|real_at| {
        let ttl_ms = agent.ttl_ms;
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
                model: agent.canonical_model.clone(),
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
/// rejected, or the entry expired meanwhile. Its body stays counted in `Inner::retained` while it lives.
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
    fn of(key: &SessionKey, agent: &Agent, state: RecipeState) -> Skipped {
        let observation = agent.current.as_ref();
        Skipped {
            key: key.clone(),
            key_generation: agent.key_generation,
            pool_id: agent.pool_id.clone(),
            account: agent.account.clone(),
            canonical_model: agent.canonical_model.clone(),
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
    pub(crate) fn of(agent: &Agent, probe: UnansweredProbe) -> Kept {
        Kept {
            key_generation: agent.key_generation,
            pool_id: agent.pool_id.clone(),
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

pub(crate) struct Inner {
    pub sessions: BTreeMap<SessionKey, Session>,
    /// Request id to the agent following it.
    pub pending: HashMap<String, (SessionKey, String)>,
    /// A compaction's request id to the agent it is open on, read before `pending` by every request event, so the
    /// observation's paths never see a compaction.
    pub compactions: HashMap<String, (SessionKey, String)>,
    /// The marks by request id, inserted before the arrival's refusals and taken at the completion before the
    /// `pending` lookup, a mark older than an hour dropped at the next insert. Never saved.
    pub contradiction_marks: HashMap<String, ChainMark>,
    /// The instant of the last contradiction line per client version, so one prints per version per hour.
    pub chain_warned: HashMap<String, i64>,
    pub reasons: BTreeMap<Reason, u64>,
    /// Every body in memory, a recipe an arrival set aside beside the current ones, since `max_retained_bytes` bounds
    /// memory.
    pub retained: usize,
    pub running: usize,
    pub closed: bool,
    pub settlements: Vec<Settlement>,
    /// The settlements the load and the lapse sweep queued, held until a save has dropped their probes from the
    /// agents: a failed save then sends nothing to the ledger, and a restart before a landed save settles each probe
    /// once rather than twice.
    pub settlements_after_save: Vec<Settlement>,
    /// The probes a gate withheld this locked section, with the keys their skip rows file under, read while the
    /// recipe still held its speed.
    pub skipped: Vec<Skipped>,
    /// Bus gaps seen by the observer. Each one discarded every observation that was still pending.
    pub bus_gaps: u64,
    /// The last gap's instant, so a reader can tell a recent gap from one long past.
    pub last_bus_gap_at: Option<i64>,
    pub next_incarnation: u64,
    /// The engine's timing, copied onto every agent it creates.
    pub timing: KeepaliveTiming,
    /// The engine's fixed bounds and its session expiry in milliseconds, held beside the timing so the state machine
    /// reads them where it applies them.
    pub limits: KeepaliveLimits,
    pub session_expiry_ms: i64,
    /// The idle-age counters by pool id and UTC day. A day older than the timing's `idle_return_retention_days` leaves
    /// at the day roll. Nothing prunes by pool: a pool that leaves keeps its days until they age out, since a range the
    /// overview reads may still cover them.
    pub idle_returns: BTreeMap<String, BTreeMap<i64, IdleReturns>>,
    /// The UTC day the idle-age counters last counted under, `i64::MIN` until the first count, which rolls.
    pub idle_returns_day: i64,
    /// The cutoff of the days the last roll dropped, for the next save to purge from the registry, cleared once a save
    /// that carried it lands.
    pub purge_counters_before: Option<i64>,
    /// The rows changed or removed since their last acknowledged save, each under the revision of its latest change.
    pub dirty: Dirty,
    /// Bumped by every touch, so a save that lands after a newer change still knows which change it carried.
    pub revision: u64,
    /// Whether the last save failed, so a failing registry is logged once per streak rather than every flush.
    pub persist_failing: bool,
    /// The request events and reports refused after the quiesce.
    pub late_updates: u64,
}

pub struct Engine {
    pub(crate) inner: Mutex<Inner>,
    pub(crate) parts: EngineParts,
    pub(crate) cancel: CancellationToken,
    pub(crate) workers: TaskTracker,
    /// How many times `step` has actually asked the router, for a test to hold against one ask per due tick.
    #[cfg(any(test, feature = "debug-hooks"))]
    router_view_calls: AtomicU64,
    /// Orders the flushes: a snapshot, its save and its acknowledgment run under it whole, so two flushes never
    /// overlap and an older batch never reaches the store after a newer one.
    pub(crate) flush_lock: Mutex<()>,
    /// How many flushes have entered, counted before the flush lock, so a test can hold a save and know a second
    /// flush is queued behind it.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub(crate) persist_entries: AtomicU64,
    /// How many flushes have read their batch, for a test to change a row only once the save it races has read it.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub(crate) persist_snapshots: AtomicU64,
    /// Woken as either count rises, which is what a test's wait on them waits on.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub(crate) persist_progress: Notify,
    /// Events a gap left queued behind it, counted down as they are applied. What arrives inside that backlog cannot
    /// be trusted: the terminal event of any request among them may be one the gap dropped. Apart from the engine's
    /// lock, so an event that takes no lock of its own counts down without one.
    gap_backlog: AtomicUsize,
}

impl Engine {
    pub fn new(parts: EngineParts) -> Arc<Engine> {
        // Every read of a stored policy the bounds now refuse answers keepalive off. The operator learns which pool
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
            inner: Mutex::new(Inner::empty(
                parts.timing,
                parts.limits.clone(),
                parts.session_expiry.as_millis() as i64,
            )),
            parts,
            cancel: CancellationToken::new(),
            workers: TaskTracker::new(),
            #[cfg(any(test, feature = "debug-hooks"))]
            router_view_calls: AtomicU64::new(0),
            flush_lock: Mutex::new(()),
            #[cfg(any(test, feature = "debug-hooks"))]
            persist_entries: AtomicU64::new(0),
            #[cfg(any(test, feature = "debug-hooks"))]
            persist_snapshots: AtomicU64::new(0),
            #[cfg(any(test, feature = "debug-hooks"))]
            persist_progress: Notify::new(),
            gap_backlog: AtomicUsize::new(0),
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

    /// The counters per pool id summed over the UTC day of `now` and the `days - 1` before it, for the overview, so
    /// the day fold stays the engine's.
    pub fn idle_returns_over(&self, now: i64, days: u32) -> BTreeMap<String, IdleReturns> {
        let last = utc_day(now);
        let first = last - i64::from(days) + 1;
        self.lock()
            .idle_returns
            .iter()
            .map(|(pool, by_day)| {
                let mut sum = IdleReturns::default();
                for counters in by_day.range(first..last + 1).map(|(_, counters)| counters) {
                    for (total, bucket) in sum.buckets.iter_mut().zip(&counters.buckets) {
                        total.reached += bucket.reached;
                        total.returned += bucket.returned;
                    }
                }
                (pool.clone(), sum)
            })
            .collect()
    }

    /// The caps the bootstrap file's `keepalive` block sets, which the sessions overview answers beside its counts.
    pub fn limits(&self) -> &KeepaliveLimits {
        &self.parts.limits
    }

    /// The pools whose policy warms caches, read off the router's snapshot as the scheduler reads a pool's policy
    /// (`pool_policies`).
    pub fn warming(&self) -> BTreeSet<String> {
        pool_policies(&self.parts.router.snapshot())
            .into_iter()
            .filter(|(_, policy)| policy.enabled)
            .map(|(pool, _)| pool)
            .collect()
    }

    pub fn stats(&self) -> EngineStats {
        let inner = self.lock();
        EngineStats {
            agents: inner.agent_count(),
            bus_gaps: inner.bus_gaps,
            last_bus_gap_at: inner.last_bus_gap_at,
            late_updates: inner.late_updates,
            live_sessions: inner.live_sessions(),
            retained_bytes: inner.retained,
            running_probes: inner.running,
            sessions: inner.sessions.len(),
        }
    }

    /// The inputs every pool's ceilings read, at this instant. The router's catalog names the dated id each undated
    /// dollar row resolves to, and a router that cannot answer leaves every row judged under its own id.
    pub async fn ceiling_inputs(&self) -> CeilingInputs {
        let now = self.now();
        let revisions = self.parts.pricing.revisions();
        let rows = FactorRows::of(revisions.in_force(now), &self.parts.tables, now);
        let undated: Vec<String> = rows
            .dollar_models()
            .filter(|model| listing::undated(model).is_none())
            .map(str::to_string)
            .collect();
        let dated = self.dated_names(undated).await;
        CeilingInputs {
            rows,
            dated,
            timing: self.parts.timing,
        }
    }

    /// Each of `models` the router's catalog resolves to another name, with that name. A router that cannot answer
    /// leaves every name as it is.
    async fn dated_names(&self, models: Vec<String>) -> HashMap<String, String> {
        self.parts
            .router
            .canonicals(models.clone())
            .await
            .map(|answered| {
                models
                    .into_iter()
                    .zip(answered)
                    .filter(|(model, dated)| model != dated)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The lease and expiry sweep. Every entry point runs it first, so a session is never read as live past
    /// its lease by a caller that happened to arrive before the ticker.
    pub fn expire(&self) {
        let now = self.now();
        {
            let mut inner = self.lock();
            inner.expire(now);
        }
        self.flush();
    }

    pub fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        let keys: Vec<SessionKey> = inner.sessions.keys().cloned().collect();
        for key in keys {
            inner.discard_session(&key, Reason::Reset, None);
        }
        inner.sessions.clear();
        inner.pending.clear();
        inner.compactions.clear();
        inner.retained = 0;
        self.cancel.cancel();
    }
}

impl Inner {
    /// A fresh engine's own state, and the fixture a unit test builds one agent onto: the one place every field
    /// is named, so a field this struct gains needs no second literal to keep in step.
    pub(crate) fn empty(
        timing: KeepaliveTiming,
        limits: KeepaliveLimits,
        session_expiry_ms: i64,
    ) -> Inner {
        Inner {
            sessions: BTreeMap::new(),
            pending: HashMap::new(),
            compactions: HashMap::new(),
            contradiction_marks: HashMap::new(),
            chain_warned: HashMap::new(),
            reasons: BTreeMap::new(),
            retained: 0,
            running: 0,
            closed: false,
            settlements: Vec::new(),
            settlements_after_save: Vec::new(),
            skipped: Vec::new(),
            bus_gaps: 0,
            last_bus_gap_at: None,
            next_incarnation: 0,
            timing,
            limits,
            session_expiry_ms,
            idle_returns: BTreeMap::new(),
            idle_returns_day: i64::MIN,
            purge_counters_before: None,
            dirty: Dirty::default(),
            revision: 0,
            persist_failing: false,
            late_updates: 0,
        }
    }

    /// The one writer of an agent's reason, so the counter behind the views never disagrees with the reason
    /// they show. Nearly every change to a recipe writes a reason, so this is where most of them become dirty.
    pub(crate) fn set_reason(&mut self, key: &SessionKey, agent_id: &str, reason: Reason) {
        if let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) {
            agent.reason = reason;
        }
        *self.reasons.entry(reason).or_default() += 1;
        self.touch_agent(key, agent_id);
    }

    /// The agents `max_agents` bounds: every record of a session that took its slots. A registered session's agents
    /// hold none until a request the caps admit.
    pub(crate) fn agent_count(&self) -> usize {
        self.sessions.values().map(Session::agent_slots).sum()
    }

    /// The sessions `max_sessions` bounds, those that hold a slot.
    pub(crate) fn live_sessions(&self) -> usize {
        self.sessions.values().filter(|s| s.holds_slot()).count()
    }

    /// Discards every agent of the session under `reason`, each one's probes settled under `wasted`.
    pub(crate) fn discard_session(
        &mut self,
        key: &SessionKey,
        reason: Reason,
        wasted: Option<Wasted>,
    ) {
        let agents: Vec<String> = self
            .sessions
            .get(key)
            .map(|session| session.agents.keys().cloned().collect())
            .unwrap_or_default();
        for agent in &agents {
            self.discard(key, agent, reason, wasted);
        }
    }

    /// Ends the session under `reason`: its agents discarded, their probes settled as `session_ended`, its rows filed
    /// for removal and its entry gone.
    fn remove_session(&mut self, key: &SessionKey, reason: Reason) {
        self.discard_session(key, reason, Some(Wasted::SessionEnded));
        self.forget_session(key);
        self.sessions.remove(key);
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
    /// for that request, so it keeps them until the first of these decides them: its successful completion (each
    /// probe by its chain's last entry: a warm return on its lineage inside that entry clears it, one past it settles
    /// it under `expired`, and any other return, on another lineage or with no read, under `cold_return`), a labeled
    /// discard (under its label), or the agent's lapse or eviction. An eviction settles them at once, and a lapse
    /// settles at once only those whose chain has ended and hands the rest back under `expired`. A request the engine
    /// takes no observation of leaves them kept for whichever comes next, and one that fails hands them back for the
    /// next turn to read or the lapse sweep to settle.
    pub(crate) fn discard_keeping_unanswered(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        reason: Reason,
    ) {
        self.end_observation(key, agent_id, None, Unanswered::Keep);
        self.set_reason(key, agent_id, reason);
    }

    /// `discard_keeping_unanswered` for an arrival, an armed recipe (unpaused, its entry ahead of `now`) set aside
    /// instead of ended: its observation leaves `current` and `pending` with its size kept in `retained`, and waits
    /// with its schedule and context for the completion to restore or drop it. A paused recipe and one that never armed
    /// end through the discard. An older recipe set aside over a completion the engine missed leaves through the
    /// discard.
    pub(crate) fn set_aside(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let aside = agent_mut(&mut self.sessions, key, agent_id)
            .filter(|agent| !agent.paused && agent.expires.is_some_and(|expires| expires > now))
            .and_then(|agent| {
                let observation = agent.current.take()?;
                Some(AsideRecipe {
                    observation,
                    expires: agent.expires,
                    retry_at: agent.retry_at,
                    jitter_frac: agent.jitter_frac,
                    ttl_ms: agent.ttl_ms,
                    cache_started: agent.cache_started,
                    failures: agent.failures,
                    canonical_model: agent.canonical_model.clone(),
                    entrypoint: agent.entrypoint.clone(),
                    account: agent.account.clone(),
                    account_incarnation: agent.account_incarnation,
                    pool_id: agent.pool_id.clone(),
                    key_generation: agent.key_generation,
                })
            });
        if let Some(aside) = &aside {
            self.pending.remove(&aside.observation.id);
        }
        self.discard_keeping_unanswered(key, agent_id, Reason::AwaitingSelection);
        if let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) {
            agent.aside = aside;
        }
    }

    /// The recipe the arrival set aside, taken out for the completion to restore, its body still counted in
    /// `retained`.
    pub(crate) fn take_aside(&mut self, key: &SessionKey, agent_id: &str) -> Option<AsideRecipe> {
        agent_mut(&mut self.sessions, key, agent_id)?.aside.take()
    }

    /// Puts the recipe `take_aside` took back into `current` with the schedule, the error count and the model and
    /// account it stood on, where its entry is still ahead of `now`, and otherwise drops it, its size leaving
    /// `retained`. Its id never goes back into `pending`: its own request ended before the one that set it aside
    /// arrived, so the lapse, the eviction and the sweeps take it as any armed recipe. Answers whether it re-armed.
    pub(crate) fn rearm_aside(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        aside: AsideRecipe,
        now: i64,
    ) -> bool {
        let agent = agent_mut(&mut self.sessions, key, agent_id)
            .filter(|_| aside.expires.is_some_and(|expires| expires > now));
        let Some(agent) = agent else {
            self.retained -= aside.observation.recipe.size;
            return false;
        };
        agent.current = Some(aside.observation);
        agent.expires = aside.expires;
        agent.retry_at = aside.retry_at;
        agent.jitter_frac = aside.jitter_frac;
        agent.ttl_ms = aside.ttl_ms;
        agent.cache_started = aside.cache_started;
        agent.failures = aside.failures;
        agent.canonical_model = aside.canonical_model;
        agent.entrypoint = aside.entrypoint;
        agent.account = aside.account;
        agent.account_incarnation = aside.account_incarnation;
        agent.pool_id = aside.pool_id;
        agent.key_generation = aside.key_generation;
        // The next save seals the restored body in place of the failed request's.
        agent.body_dirty = true;
        self.touch_agent(key, agent_id);
        true
    }

    /// Drops the recipe the arrival set aside, its size leaving `retained`.
    pub(crate) fn drop_aside(&mut self, key: &SessionKey, agent_id: &str) {
        if let Some(aside) = self.take_aside(key, agent_id) {
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
    pub(crate) fn decide_unanswered(&mut self, key: &SessionKey, agent_id: &str, read: u64) {
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        let answer = Answer {
            // The read happened when the request reached the cache, so a stream that outlives the entry still read it.
            started: agent.current.as_ref().map(|o| o.started),
            account: agent.account.clone(),
            model: agent.canonical_model.clone(),
            fast: agent.current.as_ref().is_some_and(|o| o.recipe.fast),
            read,
        };
        let probes = std::mem::take(&mut agent.kept);
        self.decide_against(key, agent_id, probes, answer);
    }

    /// `decide_unanswered`'s test over `probes` against the arrival, the lineage and the read `answer` holds: a turn's
    /// observation and its agent's, or a compaction's own. `probes` is every probe the request answers in one slice,
    /// so a chain that runs from the kept probes into the unanswered ones settles against its last entry.
    fn decide_against(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        probes: Vec<Kept>,
        answer: Answer,
    ) {
        self.touch_agent(key, agent_id);
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
        // A labeled stop settles or writes off the probes an open compaction would answer, so the compaction ends as a
        // failed one, since its completion would credit a probe the stop wasted and relabel the stop. A registration
        // that takes a released agent back follows such a stop, so it finds no compaction open.
        if wasted.is_some() {
            self.end_compaction(key, agent_id);
        }
        // Whatever ends the observation leaves nothing for a recipe set aside to wait for, so its body leaves memory
        // with it.
        self.drop_aside(key, agent_id);
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        drop_current(agent, &mut self.pending, &mut self.retained);
        agent.expires = None;
        agent.retry_at = None;
        agent.generation += 1;
        agent.paused = false;
        agent.last_stop = wasted;
        match (unanswered, wasted) {
            (Unanswered::Leave, _) => {}
            (Unanswered::Keep, _) => {
                // A row apiece: a request whose completion never came leaves its probes for the next one, which may
                // keep probes paid under other keys, and each settles by its chain's last entry. A ceiling's label
                // rides along, for the lapse sweep to settle under when no turn reads them first.
                let probes = agent.take_unanswered();
                agent.kept.extend(probes.into_iter().map(|kept| Kept {
                    pending: wasted,
                    ..kept
                }));
            }
            (Unanswered::Settle, Some(label)) => {
                self.settlements.extend(agent.settle_all(key, label));
            }
            (Unanswered::Settle, None) => agent.unanswered.clear(),
        }
    }

    /// Ends the compaction open on the agent as a failed one: it leaves the agent and the map, settles nothing and
    /// leaves the recipe as it stood, so its later events miss both maps as an unknown request's do.
    pub(crate) fn end_compaction(&mut self, key: &SessionKey, agent_id: &str) {
        let Some(open) =
            agent_mut(&mut self.sessions, key, agent_id).and_then(|agent| agent.compaction.take())
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
    pub(crate) fn settle_lapsed_pending(&mut self, now: i64) {
        let open = &self.pending;
        let mut settled: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            for (agent_id, agent) in session.agents.iter_mut() {
                // An open turn's or compaction's completion decides the probes, as `lapse` waits for it. A request that
                // fails, past a head or not, or ends at a bus gap decides nothing and leaves them here.
                // The sweep settles labeled probes alone, so an agent holding none builds no chain.
                if request_open(open, agent)
                    || !agent.kept.iter().any(|kept| kept.pending.is_some())
                {
                    continue;
                }
                let chain: Vec<ChainLink> = agent
                    .kept
                    .iter()
                    .map(Kept::link)
                    .chain(agent.unanswered.iter().map(UnansweredProbe::link))
                    .collect();
                let chain_ends: Vec<i64> = agent
                    .kept
                    .iter()
                    .map(|probe| Self::chain_expires(&chain, probe.link()))
                    .collect();
                if !agent
                    .kept
                    .iter()
                    .zip(&chain_ends)
                    .any(|(kept, &chain_end)| kept.pending.is_some() && chain_end <= now)
                {
                    continue;
                }
                let mut waiting = Vec::new();
                let probes = std::mem::take(&mut agent.kept);
                for (kept, chain_end) in probes.into_iter().zip(chain_ends) {
                    match kept.pending {
                        Some(label) if chain_end <= now => {
                            self.settlements_after_save.push(kept.settle(key, label));
                        }
                        _ => waiting.push(kept),
                    }
                }
                agent.kept = waiting;
                settled.push((key.clone(), agent_id.clone()));
            }
        }
        for (key, agent_id) in &settled {
            self.touch_agent(key, agent_id);
        }
    }

    /// A gate's hold on a retained recipe. One hold withholds one probe, whatever number of ticks it spans: the
    /// production ticker runs each second, so a five-hour cooldown would otherwise ledger 18,000 withheld probes
    /// for the one it actually withheld. A hold that begins before the due time has withheld nothing yet, so the
    /// skip waits for the due time to pass. The due time is read after the policy refresh, so the pool's current
    /// policy decides whether a probe was due, and a probe already in flight was not withheld.
    pub(crate) fn hold(&mut self, key: &SessionKey, agent_id: &str, reason: Reason, now: i64) {
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
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
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        let probes = agent.take_unanswered();
        if probes.is_empty() {
            return;
        }
        self.settlements
            .extend(probes.into_iter().map(|kept| kept.settle(key, label)));
        self.touch_agent(key, agent_id);
    }

    /// The entry is dead at its expiry and the lead already places the due time ahead of it, so a probe past it warms
    /// nothing: the recipe ends under `expired` rather than being probed, held or not, its probes written off
    /// (`lapse_probes`), and the next real request re-arms it. A probe already in flight was sent in time and settles
    /// on its own answer, a hold that already ledgered the probe it withheld is not counted again, and the skip takes
    /// the recipe's speed before the lapse drops the recipe. While a request is open on the agent, the one the
    /// observation follows or a compaction, the recipe is held unprobed instead: its completion earns its credit and
    /// decides the probes, and the next tick lapses it. A paused recipe loses its prompt alone (`drop_paused_prompt`).
    /// True when the tick is done with the recipe.
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
        if request_open(&self.pending, agent) {
            return true;
        }
        if agent.paused {
            tracing::info!(
                key = %key.0,
                session = %key.1,
                agent = %agent_id,
                expires = %phoebis_store::utc_stamp(expires),
                late_ms = now - expires,
                "a paused keepalive recipe's entry expired, so its prompt was dropped"
            );
            self.drop_paused_prompt(key, agent_id, now);
            return true;
        }
        tracing::info!(
            key = %key.0,
            session = %key.1,
            agent = %agent_id,
            expires = %phoebis_store::utc_stamp(expires),
            late_ms = now - expires,
            "a keepalive recipe lapsed before its probe, so the probe was skipped"
        );
        if !agent.hold_counted {
            // The lapse is what withholds the probe, so it counts under the state the recipe's end below leaves.
            lapsed.push(Skipped::of(
                key,
                agent,
                recipe_state(Reason::Expired, agent.paused),
            ));
        }
        self.lapse_probes(key, agent_id, now);
        self.end_observation(key, agent_id, Some(Wasted::Expired), Unanswered::Keep);
        self.set_reason(key, agent_id, Reason::Expired);
        true
    }

    /// The lapse of a paused recipe. The pause withheld every probe and holds until the next real request, so the agent
    /// keeps its pause bit and its reason for the view's `paused`, and only the prompt the cold entry made worthless
    /// leaves, with the probes it paid for written off under `expired` (`lapse_probes`). No probe was due, so no skip
    /// is ledgered.
    fn drop_paused_prompt(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        self.touch_agent(key, agent_id);
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        drop_current(agent, &mut self.pending, &mut self.retained);
        agent.expires = None;
        agent.last_stop = Some(Wasted::Expired);
        self.lapse_probes(key, agent_id, now);
    }

    /// A lapse's write-off of the agent's probes under `expired`, judged as the lapse sweep judges them: a probe whose
    /// chain's last entry (`chain_expires` over the kept and the unanswered probes) ended at or before `now` settles
    /// here, and the rest move into `kept` under the label, for the sweep to settle at their chain's end or a turn
    /// inside it to read. The recipe a failed turn restored can lapse while a probe the failed turn's own recipe sent
    /// keeps the chain alive past it.
    fn lapse_probes(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        let unanswered = agent.take_unanswered();
        let probes: Vec<Kept> = std::mem::take(&mut agent.kept)
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
                agent.kept.push(Kept {
                    pending: Some(Wasted::Expired),
                    ..kept
                });
            }
        }
    }

    /// A deadline or a wake that passed ends that part of the park, and a park with nothing left ended without a
    /// request: the idle window restarts here and main reads idle. The mod reports the same end with `idle` when its
    /// heartbeat sees it first, and whichever comes first records it. Main's end is the engine's own move out of
    /// parked, which no report repeats.
    pub(crate) fn end_passed_park(&mut self, key: &SessionKey, agent_id: &str, now: i64) {
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        let Some(held) = agent.park else {
            return;
        };
        let left = held.park.passed(now);
        if left == Some(held.park) {
            return;
        }
        agent.park = left.map(|park| HeldPark {
            began: held.began,
            park,
        });
        if left.is_some() {
            self.touch_agent(key, agent_id);
            return;
        }
        // The window restarts here, so the one counted so far ends with no return, closed before its start moves.
        agent.close_idle_episode();
        agent.idle_window_floor = None;
        agent.park_ended_at = Some(now);
        if agent.id != MAIN_AGENT {
            self.touch_agent(key, agent_id);
            return;
        }
        agent.set_state(AgentState::Idle);
        self.touch_agent(key, agent_id);
    }

    /// Drops the days the retention no longer keeps once `now` falls past the UTC day the counters last counted under,
    /// and records the cutoff for the next save, which takes the registry's rows of those days. The two sites that
    /// count call it, as `Agent::roll_counters_day` is called, and the tick is one of them, so a day leaves within a
    /// tick of the midnight that ages it out.
    fn roll_idle_returns_day(&mut self, now: i64) {
        let day = utc_day(now);
        if day <= self.idle_returns_day {
            return;
        }
        self.idle_returns_day = day;
        let retention = self.timing.idle_return_retention_days;
        if retention == 0 {
            return;
        }
        let cutoff = day - (i64::from(retention) - 1);
        let mut dropped = false;
        for days in self.idle_returns.values_mut() {
            let kept = days.split_off(&cutoff);
            dropped |= !days.is_empty();
            *days = kept;
        }
        self.idle_returns.retain(|_, days| !days.is_empty());
        if dropped {
            self.purge_counters_before = Some(cutoff);
        }
    }

    /// Advances main's idle window on every attended session: opens one where none is counted and raises `reached`
    /// up to the window's current bucket. It reads the sessions, never the retained recipes, so a session whose
    /// recipe a ceiling discarded, or that never sent a request, still reaches the later buckets. A busy main keeps
    /// its window and counts nothing, and a window whose return a prompt announced waits for the request.
    pub(crate) fn advance_idle_episodes(&mut self, snapshot: &RegistrySnapshot, now: i64) {
        self.roll_idle_returns_day(now);
        let day = utc_day(now);
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
            // Only a window that crossed into a new bucket is counted and written, so an idle session costs no save
            // per tick and a day holds no empty counters.
            if episode.bucket.is_some_and(|counted| bucket <= counted) {
                continue;
            }
            raised.push((key.clone(), episode.pool_key.clone()));
            raise_reached(
                self.idle_returns
                    .entry(episode.pool_key.clone())
                    .or_default()
                    .entry(day)
                    .or_default(),
                episode.bucket,
                bucket,
            );
            episode.bucket = Some(bucket);
            episode.bucket_day = Some(day);
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
        self.roll_idle_returns_day(now);
        let Some(main) = agent_mut(&mut self.sessions, key, MAIN_AGENT) else {
            return;
        };
        let Some(episode) = main.close_idle_episode() else {
            return;
        };
        // Never below the fence: the window already counted as reaching it, so its return is read there.
        let bucket = idle_age_bucket(episode.prompt_at.unwrap_or(now) - episode.started)
            .max(episode.bucket.unwrap_or(0));
        let today = utc_day(now);
        let raised = episode.bucket.is_none_or(|counted| bucket > counted);
        // The return's bucket is the highest the window counted, so the return counts on the day that bucket was
        // reached (today where this request raised it), and no day answers a return without its reach. Where that
        // day has left at the roll, or a window restored from a row saved without it holds its reach under no day,
        // the return leaves with its reach and counts nothing.
        let retention = i64::from(self.timing.idle_return_retention_days);
        let return_day = if raised {
            Some(today)
        } else {
            episode
                .bucket_day
                .filter(|day| retention == 0 || *day >= self.idle_returns_day - (retention - 1))
        }
        .filter(|_| continues_prefix(episode.anchor.as_ref(), boundaries));
        if raised || return_day.is_some() {
            let days = self
                .idle_returns
                .entry(episode.pool_key.clone())
                .or_default();
            if raised {
                raise_reached(days.entry(today).or_default(), episode.bucket, bucket);
            }
            if let Some(day) = return_day {
                days.entry(day).or_default().buckets[bucket].returned += 1;
            }
        }
        self.touch_agent(key, MAIN_AGENT);
        if raised || return_day.is_some() {
            self.touch_pool(&episode.pool_key);
        }
    }

    /// Marks main's refreshing request whose chain holds none of the boundaries of main's last prefix, for its
    /// completion to read against the previous prompt. A compaction and the first request after one rewrite the context
    /// the chain hashes, so their chains move with no contradiction to read.
    fn mark_chain(&mut self, key: &SessionKey, observed: &RequestObserved, now: i64) {
        let refreshing = observed
            .client
            .request_class
            .as_deref()
            .is_some_and(|class| class_guard(class) == Guard::Rearm);
        if !refreshing || observed.client.context_compacted.is_some() {
            return;
        }
        let Some(main) = agent_ref(&self.sessions, key, MAIN_AGENT) else {
            return;
        };
        let (Some(anchor), Some(previous_prompt)) =
            (main.last_prefix.as_ref(), main.last_prompt_tokens)
        else {
            return;
        };
        if continues_prefix(Some(anchor), &observed.boundaries) {
            return;
        }
        self.contradiction_marks
            .retain(|_, mark| now - mark.arrived_at <= ONE_HOUR_MS);
        self.contradiction_marks.insert(
            observed.request_id.clone(),
            ChainMark {
                session: key.clone(),
                previous_prompt,
                arrived_at: now,
            },
        );
    }

    pub(crate) fn incarnation(&mut self) -> u64 {
        self.next_incarnation += 1;
        self.next_incarnation
    }

    /// Whether a result about `agent` may still land: the session is not ended, the agent is registered, and
    /// its state admits traffic. An unattended session is live. The idle limits decide what happens to it.
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
    /// open observation took from the recipe before it. An open compaction's own and the one it took count too, since
    /// its completion compares them while the recipe it would answer may have ended.
    pub(crate) fn retained_models(&self) -> Vec<String> {
        let mut models = BTreeSet::new();
        for (key, session) in &self.sessions {
            for (agent_id, agent) in &session.agents {
                if !self.live(key, agent_id) {
                    continue;
                }
                if let Some(observation) = &agent.current {
                    models.insert(agent.canonical_model.clone());
                    if let Some(prior) = &observation.prior {
                        models.insert(prior.model.clone());
                    }
                }
                if let Some(compaction) = &agent.compaction {
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
    pub(crate) fn follow_resolutions(&mut self, canonical: &HashMap<String, String>) {
        let mut followed: Vec<(SessionKey, String)> = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            for (agent_id, agent) in session.agents.iter_mut() {
                if agent.follow_resolutions(canonical) {
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
                    && !request_open(&self.pending, agent)
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

    /// Sets the record's leave one pool cache TTL out from `now`, from the snapshot's pool row, on a record its caller
    /// set released.
    pub(crate) fn release_record(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        now: i64,
        snapshot: &RegistrySnapshot,
    ) {
        let Some(agent) = agent_mut(&mut self.sessions, key, agent_id) else {
            return;
        };
        // A pool the snapshot no longer holds, or none for a record that never sent a request, reads the default
        // policy, whose plan set keeps the hour.
        let policy = snapshot
            .pool(&agent.pool_id)
            .map(|pool| CacheTtlPolicy::from_json(&pool.cache_ttl_json))
            .unwrap_or_default();
        agent.leaves_at = Some(now + longest_cache_ttl_ms(&policy));
        self.touch_agent(key, agent_id);
    }

    pub(crate) fn expire(&mut self, now: i64) {
        // A record whose probe is still in flight waits for the result, which settles under the release's label, and
        // the next sweep takes it.
        let leaving: Vec<(SessionKey, String)> = self
            .sessions
            .iter()
            .flat_map(|(key, session)| {
                session
                    .agents
                    .iter()
                    .filter(|(_, agent)| {
                        agent.state == AgentState::Released
                            && !agent.in_flight
                            && agent.leaves_at.is_some_and(|at| now >= at)
                    })
                    .map(move |(agent_id, _)| (key.clone(), agent_id.clone()))
            })
            .collect();
        for (key, agent_id) in leaving {
            self.forget_agent(&key, &agent_id);
            if let Some(session) = self.sessions.get_mut(&key) {
                session.agents.remove(&agent_id);
            }
            self.touch_session(&key);
        }
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
            .filter(|(_, s)| {
                now - s.last_real_at.unwrap_or(0).max(s.last_report_at) >= self.session_expiry_ms
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.remove_session(&key, Reason::SessionExpired);
        }
    }

    /// Ends every session whose key the registry no longer holds under the generation that registered it: a
    /// deletion, or the same id created again, takes the previous holder's sessions and recipes with it rather
    /// than leaving them to the session expiry.
    pub(crate) fn drop_sessions_of_dead_keys(&mut self, snapshot: &RegistrySnapshot) {
        let dead: Vec<SessionKey> = self
            .sessions
            .iter()
            .filter(|(key, s)| snapshot.key(&key.0).map(|k| k.created_at) != Some(s.key_generation))
            .map(|(k, _)| k.clone())
            .collect();
        for key in dead {
            self.remove_session(&key, Reason::SessionEnded);
        }
    }
}

/// The longest TTL the policy names in milliseconds, over both rule sets' `main`, `subagent` and model overrides.
pub(crate) fn longest_cache_ttl_ms(policy: &CacheTtlPolicy) -> i64 {
    // Whatever `enabled` says: the rules are what a body's markers are completed against, so a resumed agent's next
    // request can land under the longest of them.
    [&policy.plan, &policy.metered]
        .into_iter()
        .flat_map(|rules| {
            [rules.main, rules.subagent]
                .into_iter()
                .chain(rules.models.iter().flat_map(|m| [m.main, m.subagent]))
        })
        .map(CacheTtl::ms)
        .fold(FIVE_MINUTES_MS, i64::max)
}

/// The engine's bus task. Every event is handed to `observe` under the lock. Nothing here awaits while holding
/// it, so a slow registry read never stalls the queue. The bus drops for a subscriber that falls behind and
/// counts it. A rise in that counter means events this task never saw, so every observation still open is
/// discarded before the next event is applied.
pub fn spawn_observer(engine: Arc<Engine>) -> tokio::task::JoinHandle<()> {
    // Best-effort: a drop reads as a gap below, which discards the observations it could have misled. The queue holds
    // the kinds the engine reads, so a burst of feature calls, which move nothing it keeps warm, raises no gap.
    let subscriber =
        engine
            .parts
            .bus
            .subscribe_filtered("keepalive", Delivery::BestEffort, |event| {
                !matches!(event, BusEvent::FeatureCall(_))
            });
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
        // A chunk, a head and a selection push nothing to settle or to skip, so they flush nothing.
        let flushes = matches!(
            event,
            BusEvent::RequestObserved(_) | BusEvent::RequestComplete(_)
        );
        match event {
            BusEvent::RequestObserved(observed) => {
                let catalog = self.parts.relay.catalog();
                self.on_observed(observed, |spelling| catalog.lookup(spelling));
            }
            BusEvent::AccountSelected(selected) => self.on_selected(selected),
            BusEvent::ResponseHead(head) => self.on_head(head),
            BusEvent::ResponseChunk(chunk) => self.on_chunk(chunk),
            BusEvent::RequestComplete(complete) => self.on_complete(complete),
            // A feature call carries no session and no model, so it moves nothing the engine keeps warm.
            BusEvent::FeatureCall(_) => {}
        }
        self.counted_down();
        if flushes {
            self.flush();
        }
    }

    /// One event of the backlog applied. The count is read before the event's arm rather than after, so the event a
    /// gap was noticed on is itself inside its own backlog.
    fn counted_down(&self) {
        let _ = self
            .gap_backlog
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }

    /// Events were lost between the last one seen and this one. Whatever was still open cannot be completed
    /// with confidence: the lost events may have been the completion, or a failover onto another account. An
    /// armed recipe's evidence was complete before the gap and stays.
    pub fn bus_gap(&self, dropped: u64, backlog: usize) {
        let mut inner = self.lock();
        inner.bus_gaps += 1;
        inner.last_bus_gap_at = Some(self.now());
        self.gap_backlog.store(backlog, Ordering::SeqCst);
        tracing::warn!(
            dropped,
            "the keepalive subscriber fell behind the bus, so the pending observations were discarded"
        );
        let open: Vec<(SessionKey, String)> = inner.pending.values().cloned().collect();
        for (key, agent_id) in open {
            // The lost events may have been this turn's completion, so it counts as the turn that answered, as a
            // missed one does at the next arrival.
            if let Some(agent) = agent_mut(&mut inner.sessions, &key, &agent_id) {
                if let Some(started) = agent.current.as_ref().map(|o| o.started) {
                    agent.last_answered_at = Some(started);
                }
            }
            inner.end_observation(&key, &agent_id, None, Unanswered::Leave);
            inner.set_reason(&key, &agent_id, Reason::BusGap);
        }
        // A compaction's completion may be among the lost events too, and an open compaction holds its agent's recipe
        // and sweep until it completes, so each ends as a failed one.
        for (key, agent_id) in std::mem::take(&mut inner.compactions).into_values() {
            inner.end_compaction(&key, &agent_id);
        }
    }

    pub fn parts_bus(&self) -> Bus {
        self.parts.bus.clone()
    }

    fn on_observed(&self, observed: RequestObserved, resolve: impl Fn(&str) -> Option<String>) {
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
        let key: SessionKey = (key_id, session_id);
        // Read ahead of the body's parse, so a request on a pool with the keepalive off or from an opted-out key keeps
        // no body. A key the snapshot lacks reads the default policy, which is off.
        let arrival_refusal = {
            let snapshot = self.parts.router.snapshot();
            let key_row = snapshot.key(&key.0);
            let enabled = key_row
                .and_then(|row| snapshot.pool(&row.pool_id))
                .is_some_and(|pool| KeepalivePolicy::from_json(&pool.keepalive_json).enabled);
            if !enabled {
                Some(Reason::KeepaliveOff)
            } else if key_row.is_some_and(|row| !row.companion_permissions.keepalive) {
                Some(Reason::OptedOut)
            } else {
                None
            }
        };
        // The body's parse and edits run apart from the lock, which the bus observer, the tick and the companion
        // route all take, for a request on an agent the engine holds and of a class that can arm a recipe: a
        // compaction stores none. Every check below runs again under the lock, which builds the recipe itself where
        // none was built here.
        let held = {
            let inner = self.lock();
            !inner.closed && agent_ref(&inner.sessions, &key, &agent_id).is_some()
        };
        let build = || {
            make_recipe(
                &observed.headers,
                &observed.body,
                &observed.model,
                self.parts.max_request_bytes,
            )
        };
        let prebuilt = (held
            && arrival_refusal.is_none()
            && observed
                .client
                .request_class
                .as_deref()
                .is_none_or(|class| class_guard(class) == Guard::Rearm))
        .then(build);
        let mut inner = self.lock();
        if inner.closed {
            // A request on a session the engine holds is one the last save can no longer carry.
            if inner.sessions.contains_key(&key) {
                inner.late_updates += 1;
            }
            return;
        }
        inner.expire(now);
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
            || agent
                .compaction
                .as_ref()
                .is_some_and(|c| c.request_id == observed.request_id)
        {
            return;
        }
        // The header is an optional hardening on top of the registration rule: when both sides name an owner,
        // they must agree while the holder's lease is live. Foreign traffic proves nothing about this agent, so its
        // status, its recipe and its schedule are left as they were, and only the mismatch itself is counted.
        if let (Some(registered), Some(sent)) = (&session.owner_id, &observed.owner_id) {
            if registered != sent {
                // A restored session's window reads as live, since the mod re-adopts the session within it. Past the
                // lease the registered owner reports nothing, and a process that reloaded under a new owner would
                // otherwise arm nothing until the idle limit ended the session.
                if session.attended_at(now) {
                    *inner.reasons.entry(Reason::OwnerMismatch).or_default() += 1;
                    let session = inner.sessions.get_mut(&key).unwrap();
                    session.owner_mismatches += 1;
                    if session.owner_mismatches == 1 {
                        tracing::warn!(
                            session = %key.1,
                            "a request named another owner than the session's registration"
                        );
                    }
                    return;
                }
                // A request carries no instance, so the takeover records the owner alone. The touch saves it
                // whichever way the request leaves below.
                let sent = sent.clone();
                inner.sessions.get_mut(&key).unwrap().owner_id = Some(sent);
                inner.touch_session(&key);
            }
        }
        // Read again past the takeover's write.
        let agent = &inner.sessions[&key].agents[&agent_id];
        // The hijack guard: an auxiliary or foreign request must not touch the recipe, the schedule, or the
        // unanswered probes a re-arming request's credit depends on. `reason` stays the recipe's gating
        // state, since a hold's `began` check, the completion's gate-reason match, and the view's armed-or-held
        // state all read that field as state. The agent's own `last_guard` carries the verdict itself, every
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
        agent_mut(&mut inner.sessions, &key, &agent_id)
            .expect(AGENT_HELD)
            .last_guard = Some(verdict);
        // One touch for everything below: the lock is held to the end, so the flush reads the agent as this request
        // leaves it.
        inner.touch_agent(&key, &agent_id);
        // Ahead of every refusal below, so a request whose recipe the engine refuses or cannot hold still reaches the
        // watch at its completion, and ahead of the store of this request's own prefix, since the mark reads the one
        // before it.
        if agent_id == MAIN_AGENT {
            inner.mark_chain(&key, &observed, now);
        }
        // Ahead of the return below, so a foreign request on a new prefix ends the window too. The return is judged
        // against the prefix the window opened on, never on the verdict, which re-arms on any chain once a ceiling
        // has discarded the recipe.
        if agent_id == MAIN_AGENT && verdict != Guard::Auxiliary {
            inner.end_idle_episode(&key, &observed.boundaries, now);
        }
        if matches!(verdict, Guard::Auxiliary | Guard::Foreign) {
            return;
        }
        // A turn or a second compaction moves the agent past the context an open compaction was reading, so that one
        // ends as a failed one and leaves the recipe to this request's own path.
        inner.end_compaction(&key, &agent_id);
        // A tick asks the router only about the models of retained recipes and of open compactions, so a probe an
        // agent keeps past its recipe's end can miss a resolution: the catalog's acknowledged resolution dates the held
        // probes' spellings here, the request's own pair first, before the credit compares its model with the
        // request's, a compaction's as a turn's.
        let mut resolved = HashMap::new();
        if observed.wire_model != observed.canonical_model {
            resolved.insert(
                observed.wire_model.clone(),
                observed.canonical_model.clone(),
            );
        }
        for held in inner.sessions[&key].agents[&agent_id].held_models() {
            if resolved.contains_key(&held) {
                continue;
            }
            if let Some(dated) = resolve(&held).filter(|dated| *dated != held) {
                resolved.insert(held, dated);
            }
        }
        agent_mut(&mut inner.sessions, &key, &agent_id)
            .expect(AGENT_HELD)
            .follow_resolutions(&resolved);
        if verdict == Guard::Compaction {
            // The recipe, its schedule and its probes wait as they stood, so a compaction that fails leaves the old
            // context the live one. Nothing of the turn's admission below runs: the idle window's prefix, the agent's
            // activity and model, and the agent cap all belong to the turn after it.
            let agent = &inner.sessions[&key].agents[&agent_id];
            let compaction = OpenCompaction {
                request_id: observed.request_id.clone(),
                received_at: observed.received_at,
                started: now,
                canonical_model: observed.canonical_model.clone(),
                fast: observed.shape.speed.as_deref() == Some("fast"),
                attempt: 0,
                account: None,
                funding: AttemptFunding::default(),
                ok_head: false,
                read: 0,
                ttl_ms: 0,
                prior: prior_coverage(agent, now),
                client: RowClient {
                    client: Some(observed.client.class.clone()),
                    user_agent: user_agent_of(&observed.headers),
                },
            };
            agent_mut(&mut inner.sessions, &key, &agent_id)
                .expect(AGENT_HELD)
                .compaction = Some(compaction);
            inner
                .compactions
                .insert(observed.request_id, (key, agent_id));
            return;
        }
        // Apart from the recipe, which keepalive off, an opt-out or a refused body leaves unretained: the next idle
        // window judges a return against this request's prefix either way. A request without both hashes leaves none.
        if agent_id == MAIN_AGENT {
            agent_mut(&mut inner.sessions, &key, &agent_id)
                .expect(AGENT_HELD)
                .last_prefix = observed
                .static_hash
                .clone()
                .zip(observed.boundaries.last().cloned())
                .map(|(static_hash, last)| PrefixAnchor {
                    static_hash,
                    last,
                    before_last: observed
                        .boundaries
                        .len()
                        .checked_sub(2)
                        .map(|at| observed.boundaries[at].clone()),
                    marked: observed
                        .marked
                        .iter()
                        .rev()
                        .find(|at| **at + 2 < observed.boundaries.len())
                        .and_then(|at| observed.boundaries.get(*at).cloned()),
                });
        }
        // A turn still pending here had no completion observed, since a success takes it off `pending` and a failure
        // ends the observation, and `set_aside` below ends it, or sets it aside, without one. It counts as the turn
        // that answered, or the gap this arrival's coverage reads would run from a turn before it. A compaction ends no
        // turn, so it returned above and leaves the open one to its own completion.
        let missed = inner.sessions[&key].agents[&agent_id]
            .current
            .as_ref()
            .filter(|observation| inner.pending.contains_key(&observation.id))
            .map(|observation| observation.started);
        if let Some(started) = missed {
            agent_mut(&mut inner.sessions, &key, &agent_id)
                .expect(AGENT_HELD)
                .last_answered_at = Some(started);
        }
        // Taken before anything below overwrites it and consumed with the observation.
        let prior = prior_coverage(&inner.sessions[&key].agents[&agent_id], now);
        let poisoned = self.gap_backlog.load(Ordering::SeqCst) > 0;
        // The recipe set aside waits for this request's completion, so each return below that takes no observation
        // drops it: nothing would complete to restore it.
        inner.set_aside(&key, &agent_id, now);
        // A request refused the slot below moves the session's request time too, which is saved, since the expiry
        // counts from it across a restart.
        let session = inner.sessions.get_mut(&key).unwrap();
        session.last_real_at = Some(now);
        let slotted = session.slotted;
        inner.touch_session(&key);
        // A session takes its slots at the first real request the caps admit, so both caps are read here at each of
        // its requests until then, the session cap first as a registration reads them, and the session's agents join
        // the agent count only where they all fit. A request over either cap is refused the slot as a registration
        // over it is: it keeps nothing and leaves `slotted` false, so the session's next request reads the caps again.
        if !slotted {
            let joining = inner.sessions[&key].agents.len();
            let refused = if inner.live_sessions() >= inner.limits.max_sessions {
                Some(Reason::SessionLimit)
            } else if inner.agent_count() + joining > inner.limits.max_agents {
                Some(Reason::AgentLimit)
            } else {
                None
            };
            if let Some(reason) = refused {
                // The agent saw a real request, so the view lists its line with the cause, and `slotted` stays false,
                // so the session holds no slot.
                let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
                agent.last_real_at = Some(now);
                agent.canonical_model = observed.canonical_model.clone();
                inner.drop_aside(&key, &agent_id);
                inner.set_reason(&key, &agent_id, reason);
                return;
            }
            inner.sessions.get_mut(&key).unwrap().slotted = true;
        }
        let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
        if agent.state == AgentState::Unknown {
            inner.drop_aside(&key, &agent_id);
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
        agent.cold = 0;
        agent.failures = 0;
        agent.hold_counted = false;
        agent.last_real_at = Some(now);
        agent.canonical_model = observed.canonical_model.clone();
        agent.key_generation = observed.key_generation;
        // Past the activity writes and the slot, which such a request takes as any other does, and ahead of the build.
        if let Some(reason) = arrival_refusal {
            inner.drop_aside(&key, &agent_id);
            inner.set_reason(&key, &agent_id, reason);
            return;
        }
        if inner.pending.len() >= inner.limits.max_agents {
            inner.drop_aside(&key, &agent_id);
            inner.set_reason(&key, &agent_id, Reason::RequestLimit);
            return;
        }
        let built = prebuilt.unwrap_or_else(build);
        let mut recipe = match built {
            Ok(recipe) => recipe,
            Err(reason) => {
                inner.drop_aside(&key, &agent_id);
                inner.set_reason(&key, &agent_id, reason);
                return;
            }
        };
        // What the next observation's guard compares against: this request's own two hashes, not the ones a
        // prior arm happened to leave behind.
        recipe.static_hash = observed.static_hash.clone();
        recipe.tail_hash = observed.boundaries.last().cloned();
        // The agent's own aside recipe yields first, so the bound reads as it would with that recipe ended, and the
        // eviction never reaches it: an agent with a request open is no candidate.
        let max_retained_bytes = inner.limits.max_retained_bytes;
        if inner.retained + recipe.size > max_retained_bytes {
            inner.drop_aside(&key, &agent_id);
        }
        if inner.retained + recipe.size > max_retained_bytes
            && !inner.evict_for(max_retained_bytes, recipe.size)
        {
            inner.set_reason(&key, &agent_id, Reason::StorageLimit);
            return;
        }
        inner.retained += recipe.size;
        let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
        // The probe replays this request's headers, so it meters under the entrypoint they carry.
        agent.entrypoint =
            phoebis_ledger::entrypoint_of_client(&observed.client.class).map(String::from);
        agent.current = Some(Observation {
            id: observed.request_id.clone(),
            attempt: 0,
            started: now,
            received_at: observed.received_at,
            recipe,
            ready: false,
            expected: 0,
            ttl_ms: 0,
            prior,
            poisoned,
            dispatched_ms: None,
            funding: AttemptFunding::default(),
            client_label: Some(observed.client.class.clone()),
        });
        agent.body_dirty = true;
        // The mark is retired only by a save at the row's revision, so the new body takes a revision of its own.
        inner.touch_agent(&key, &agent_id);
        inner.pending.insert(observed.request_id, (key, agent_id));
    }

    fn on_selected(&self, selected: AccountSelected) {
        let funding = AttemptFunding {
            kind: AccountKind::parse(&selected.account_kind),
            usage_based: selected.usage_based,
            in_overage: false,
        };
        let snapshot = self.parts.router.snapshot();
        let mut inner = self.lock();
        if inner.compactions.contains_key(&selected.request_id) {
            if inner.quiesced() {
                return;
            }
            if let Some(agent) = inner.compaction_agent(&selected.request_id) {
                // A body the relay sent as it came carries the conversation's own markers, whose TTL the recipe's
                // evidence settled on.
                let ttl_ms = selected.cache_ttl_ms.unwrap_or(agent.ttl_ms);
                if let Some(compaction) = agent.compaction.as_mut() {
                    compaction.attempt = selected.attempt;
                    compaction.account = Some(selected.account_id);
                    compaction.funding = funding;
                    compaction.ok_head = false;
                    compaction.read = 0;
                    compaction.ttl_ms = ttl_ms;
                }
            }
            return;
        }
        let Some((key, agent_id)) = inner.followed(&selected.request_id) else {
            return;
        };
        if !inner.live(&key, &agent_id) {
            return;
        }
        let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
        let Some(observation) = agent.current.as_mut() else {
            return;
        };
        observation.attempt = selected.attempt;
        observation.ready = false;
        observation.expected = 0;
        observation.ttl_ms = 0;
        observation.dispatched_ms = selected.cache_ttl_ms;
        observation.funding = funding;
        agent.cache_started = observation.started;
        agent.expires = None;
        inner.set_reason(&key, &agent_id, Reason::AwaitingSelection);
        // The arrival reads the pool's policy and the key's opt-out, so a request refused by either never reaches here.
        if snapshot.account(&selected.account_id).is_none() {
            inner.set_reason(&key, &agent_id, Reason::IneligibleCredential);
            return;
        }
        let pool_id = snapshot.key(&key.0).map(|row| row.pool_id.clone());
        let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
        if let Some(observation) = agent.current.as_mut() {
            // A poisoned observation never becomes ready, so no evidence of this request can arm a schedule.
            observation.ready = !observation.poisoned;
        }
        if agent.account.as_deref() != Some(selected.account_id.as_str()) {
            agent.cold = 0;
            agent.ttl_ms = 0;
        }
        agent.account = Some(selected.account_id);
        // The event's own incarnation, never the snapshot's above: an id paired again between the selection and this
        // delivery would label the recipe with the replacement.
        agent.account_incarnation = Some(selected.incarnation);
        // A key deleted since the arrival leaves the agent's pool as it stood, and the next tick's
        // `drop_sessions_of_dead_keys` ends the whole session under `session_ended`.
        if let Some(pool_id) = pool_id {
            agent.pool_id = pool_id;
        }
        inner.set_reason(&key, &agent_id, Reason::AwaitingCacheEvidence);
    }

    fn on_head(&self, head: ResponseHead) {
        let in_overage = phoebis_relay::UnifiedHeaders::parse_list(&head.headers)
            .and_then(|unified| unified.overage_in_use)
            == Some(true);
        let mut inner = self.lock();
        if inner.compactions.contains_key(&head.request_id) {
            if inner.quiesced() {
                return;
            }
            if let Some(compaction) = inner
                .compaction_agent(&head.request_id)
                .and_then(|agent| agent.compaction.as_mut())
                .filter(|compaction| compaction.attempt == head.attempt)
            {
                compaction.funding.in_overage = in_overage;
                compaction.ok_head = (200..300).contains(&head.status);
            }
            return;
        }
        let Some((key, agent_id)) = inner.followed(&head.request_id) else {
            return;
        };
        if !inner.live(&key, &agent_id) {
            return;
        }
        let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
        if let Some(observation) = agent.current.as_mut() {
            if observation.attempt == head.attempt {
                let moved = observation.funding.in_overage != in_overage;
                observation.funding.in_overage = in_overage;
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
        let mut inner = self.lock();
        if inner.compactions.contains_key(&chunk.request_id) {
            if inner.quiesced() {
                return;
            }
            // A failed attempt's stream is no evidence about the cache, as for a turn.
            if let Some(compaction) = inner
                .compaction_agent(&chunk.request_id)
                .and_then(|agent| agent.compaction.as_mut())
                .filter(|compaction| compaction.attempt == chunk.attempt && compaction.ok_head)
            {
                compaction.read = compaction.read.max(usage.read);
                // The credit files under the TTL the compaction's own write would take, which its usage proves over
                // the selection's figure, as a turn's `observe_usage` settles it.
                compaction.ttl_ms = usage.ttl_ms(compaction.ttl_ms, 0);
            }
            return;
        }
        let Some((key, agent_id)) = inner.followed(&chunk.request_id) else {
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
        inner.observe_usage(&key, &agent_id, usage);
    }

    fn on_complete(&self, complete: RequestComplete) {
        let now = self.now();
        let mut inner = self.lock();
        if inner.compactions.contains_key(&complete.request_id) {
            if inner.quiesced() {
                return;
            }
            let credit = inner.complete_compaction(&complete, now);
            self.book_credit(inner, credit, &complete);
            return;
        }
        if let Some(mark) = inner.contradiction_marks.remove(&complete.request_id) {
            self.read_chain_mark(&mut inner, mark, &complete, now);
        }
        let Some((key, agent_id)) = inner.followed(&complete.request_id) else {
            return;
        };
        inner.touch_agent(&key, &agent_id);
        if agent_id == MAIN_AGENT {
            if let Some(prompt) = reported_prompt(complete.usage.as_ref()) {
                if let Some(main) = agent_mut(&mut inner.sessions, &key, MAIN_AGENT) {
                    main.last_prompt_tokens = Some(prompt);
                }
            }
        }
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
            // A failed or canceled turn leaves the last answered recipe armed as its arrival set it aside, and a
            // rejected one is a pause, which `end_observation` below makes by dropping what the arrival set aside.
            let aside = match reason {
                Reason::RequestCanceled | Reason::RequestFailed => {
                    inner.take_aside(&key, &agent_id)
                }
                _ => None,
            };
            // A failed turn is no evidence about the cache, past a head or not, so it decides none of the probes it
            // kept and hands them back for the next turn to read. A label the turn found stays, and a probe that
            // carries none takes `expired`, which the lapse sweep settles it under when its chain's last entry ends if
            // no turn reads it first, as its recipe's own lapse would have. The sweep reads that chain through the
            // restored recipe's probes, so a kept probe whose entry they read back waits for the turn that reads them.
            if let Some(agent) = agent_mut(&mut inner.sessions, &key, &agent_id) {
                for kept in agent.kept.iter_mut().filter(|kept| kept.pending.is_none()) {
                    kept.pending = Some(Wasted::Expired);
                }
            }
            // A probe sent while this request was open pays for the request after it, whatever this one did.
            inner.end_observation(&key, &agent_id, None, Unanswered::Leave);
            if let Some(aside) = aside {
                inner.rearm_aside(&key, &agent_id, aside, now);
            }
            inner.set_reason(&key, &agent_id, reason);
            return;
        }
        inner.pending.remove(&complete.request_id);
        // The answered turn's own recipe replaces the one its arrival set aside.
        inner.drop_aside(&key, &agent_id);
        // Read from the observation's own snapshot of what the previous recipe had proved: a probe that ran
        // while this request was open, or a TTL this request's evidence shortens below, cannot reach into it.
        let mut credit = {
            let agent = &inner.sessions[&key].agents[&agent_id];
            let fast = agent.current.as_ref().is_some_and(|o| o.recipe.fast);
            agent
                .current
                .as_ref()
                .and_then(|o| o.prior.as_ref())
                .filter(|prior| {
                    prior.credits(
                        agent.last_real_at.unwrap_or(now),
                        &agent.canonical_model,
                        fast,
                        agent.account.as_deref(),
                        read,
                    )
                })
                .map(|_| {
                    let served = agent.current.as_ref();
                    Credit {
                        // Its receipt picks the hour the credit files in and the revision its band and its saving
                        // price at, as its own request row's does, though the answer may land past either boundary.
                        at: served.map_or(now, |o| o.received_at),
                        key: key.clone(),
                        key_generation: agent.key_generation,
                        pool_id: agent.pool_id.clone(),
                        account: agent.account.clone(),
                        model: agent.canonical_model.clone(),
                        read,
                        // Set below, once this request's own evidence has settled the TTL.
                        ttl_ms: 0,
                        fast,
                        funding: served.map(|o| o.funding).unwrap_or_default(),
                        client: RowClient {
                            client: served.and_then(|o| o.client_label.clone()),
                            user_agent: served.and_then(|o| user_agent_of(&o.recipe.headers)),
                        },
                    }
                })
        };
        // Set once the credit has read its gap from the turn before, so the next request's gap runs from this one.
        if (200..300).contains(&complete.status) {
            if let Some(agent) = agent_mut(&mut inner.sessions, &key, &agent_id) {
                if let Some(request_started) = agent.current.as_ref().map(|o| o.started) {
                    agent.last_answered_at = Some(request_started);
                }
            }
        }
        inner.decide_unanswered(&key, &agent_id, read);
        let (ready, armed) = agent_ref(&inner.sessions, &key, &agent_id)
            .map(|a| {
                (
                    a.current.as_ref().is_some_and(|o| o.ready),
                    a.current.as_ref().is_some_and(|o| o.expected > 0),
                )
            })
            .unwrap_or((false, false));
        if ready && !armed {
            if let Some(usage) = complete.usage.as_ref().map(ProbeUsage::from_bus) {
                inner.observe_usage(&key, &agent_id, usage);
            }
        }
        let (proved, observed_ttl, poisoned, current) = agent_ref(&inner.sessions, &key, &agent_id)
            .map_or((false, 0, false, None), |a| {
                let o = a.current.as_ref();
                (
                    o.is_some_and(|o| o.ready && o.expected > 0),
                    o.map_or(0, |o| o.ttl_ms),
                    o.is_some_and(|o| o.poisoned),
                    Some(a.reason),
                )
            });
        // The TTL this request's own evidence settled on, set at a stream's first event or, for an answer without a
        // stream, by the reading just above. The write a credited request avoided would have taken it, and a discard
        // below clears the observation.
        if let Some(credit) = credit.as_mut() {
            credit.ttl_ms = observed_ttl;
        }
        if !proved {
            if poisoned {
                // The gap that poisoned it is what this request proved nothing under.
                inner.discard(&key, &agent_id, Reason::BusGap, None);
            } else {
                // The credential's standing does not vary between attempts of one request, and the pool's toggle
                // reaches this arm through the tick's hold, so ending the observation must not recount a reason the
                // request already carries.
                match current {
                    Some(Reason::KeepaliveOff | Reason::IneligibleCredential) if !ready => {
                        inner.end_observation(&key, &agent_id, None, Unanswered::Settle);
                    }
                    _ => inner.discard(&key, &agent_id, Reason::NoCacheEvidence, None),
                }
            }
        }
        if credit.is_some() {
            if let Some(agent) = agent_mut(&mut inner.sessions, &key, &agent_id) {
                agent.count_credit(now);
            }
        }
        self.book_credit(inner, credit, &complete);
    }

    /// The chain watch: a marked request whose completion read the cache at or above `CHAIN_CONTRADICTION_COVERAGE` of
    /// the previous main prompt contradicts the router's chain model, since the upstream read as a continuation what
    /// the chain called a new prefix. Every contradiction counts, one line per client version per hour names it, and
    /// the completion's own prompt becomes the one the next mark reads.
    fn read_chain_mark(
        &self,
        inner: &mut Inner,
        mark: ChainMark,
        complete: &RequestComplete,
        now: i64,
    ) {
        let read = complete
            .usage
            .as_ref()
            .and_then(|u| u.cache_read_tokens)
            .unwrap_or(0);
        if read as f64 >= mark.previous_prompt as f64 * CHAIN_CONTRADICTION_COVERAGE {
            let version = complete.client_version.as_deref().unwrap_or("unknown");
            self.parts.metrics.observe_chain_contradiction(version);
            let printed = inner
                .chain_warned
                .get(version)
                .is_some_and(|at| now - at < ONE_HOUR_MS);
            if !printed {
                inner.chain_warned.insert(version.to_string(), now);
                tracing::warn!(
                    client_version = %version,
                    session = %mark.session.1,
                    cache_read_tokens = read,
                    previous_prompt_tokens = mark.previous_prompt,
                    "a refreshing request's chain holds none of the anchor's boundaries while its completion read \
                     the cache as a continuation would"
                );
            }
        }
        // After the quiesce the last save holds the agent as it stood then.
        if inner.closed {
            return;
        }
        if let Some(prompt) = reported_prompt(complete.usage.as_ref()) {
            if let Some(main) = agent_mut(&mut inner.sessions, &mark.session, MAIN_AGENT) {
                main.last_prompt_tokens = Some(prompt);
            }
            inner.touch_agent(&mark.session, MAIN_AGENT);
        }
    }

    /// Books the credit a completion earned and releases the lock on the way.
    fn book_credit(
        &self,
        inner: std::sync::MutexGuard<'_, Inner>,
        credit: Option<Credit>,
        complete: &RequestComplete,
    ) {
        drop(inner);
        let Some(credit) = credit else {
            return;
        };
        // The tier the router's snapshot prices the serving account by, the one the request row files under, so a
        // stored term the router has not loaded yet leaves the credit beside its row.
        let snapshot = self.parts.router.snapshot();
        let tier = credit
            .account
            .as_deref()
            .and_then(|account| snapshot.priced_tier(account))
            .map(String::from);
        // The band the request's own row keys, read off its prompt as the ledger reads the serving attempt's.
        // A completion with no usage reads as a prompt of 0, which every row bands as standard.
        let band = self.parts.pricing.revisions().in_force(credit.at).band_of(
            &credit.model,
            reported_prompt(complete.usage.as_ref()).unwrap_or(0),
        );
        self.record_kept_warm(&credit, &complete.request_id, tier, band);
    }
}

impl Inner {
    /// Whether the engine is closed, counting the event that meets it as a late update.
    fn quiesced(&mut self) -> bool {
        if self.closed {
            self.late_updates += 1;
        }
        self.closed
    }

    /// The agent following `request_id`, `None` for a request no agent follows and for any once the engine is
    /// closed. After the quiesce the last save holds each agent as it stood then, so a later event is refused and
    /// counted, where `live` would refuse it uncounted: a completion earns no credit and arms nothing, and the relay
    /// has answered the client either way.
    fn followed(&mut self, request_id: &str) -> Option<(SessionKey, String)> {
        let followed = self.pending.get(request_id).cloned()?;
        if self.quiesced() {
            return None;
        }
        Some(followed)
    }

    /// The agent holding the compaction `request_id` opened while it may still take a result, `None` for one its
    /// agent no longer holds, as for a request the engine never followed.
    fn compaction_agent(&mut self, request_id: &str) -> Option<&mut Agent> {
        let (key, agent_id) = self.compactions.get(request_id)?.clone();
        if !self.live(&key, &agent_id) {
            return None;
        }
        agent_mut(&mut self.sessions, &key, &agent_id).filter(|agent| {
            agent
                .compaction
                .as_ref()
                .is_some_and(|compaction| compaction.request_id == request_id)
        })
    }

    /// The completion of an open compaction. Only a complete answer (`Outcome::Succeeded`) decides anything: every
    /// probe the recipe paid for, the kept ones and then the unanswered ones as one slice in exchange order, is judged
    /// against the compaction's own arrival, lineage and read, its kept-warm credit is its own, and the recipe stops
    /// under `compacted`, since it keeps a context the next turn never reads, so no probe goes out until that turn
    /// re-arms. Any other outcome leaves the recipe, its schedule and its probes as they stood, the old context the
    /// live one. The credit it earned comes back for the caller to book.
    fn complete_compaction(&mut self, complete: &RequestComplete, now: i64) -> Option<Credit> {
        let (key, agent_id) = self.compactions.remove(&complete.request_id)?;
        let live = self.live(&key, &agent_id);
        let agent = agent_mut(&mut self.sessions, &key, &agent_id)?;
        let compaction = agent
            .compaction
            .take_if(|compaction| compaction.request_id == complete.request_id)?;
        if !live || complete.outcome != Outcome::Succeeded {
            return None;
        }
        let read = complete
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_tokens)
            .unwrap_or(compaction.read);
        // An answer without a stream proves its TTL here alone, as the stream's phases do in `on_chunk`.
        let ttl_ms = complete.usage.as_ref().map_or(compaction.ttl_ms, |usage| {
            ProbeUsage::from_bus(usage).ttl_ms(compaction.ttl_ms, 0)
        });
        let mut probes = std::mem::take(&mut agent.kept);
        probes.extend(agent.take_unanswered());
        let credit = compaction
            .prior
            .as_ref()
            .filter(|prior| {
                prior.credits(
                    compaction.started,
                    &compaction.canonical_model,
                    compaction.fast,
                    compaction.account.as_deref(),
                    read,
                )
            })
            .map(|_| Credit {
                at: compaction.received_at,
                key: key.clone(),
                key_generation: agent.key_generation,
                pool_id: agent.pool_id.clone(),
                account: compaction.account.clone(),
                model: compaction.canonical_model.clone(),
                read,
                ttl_ms,
                fast: compaction.fast,
                funding: compaction.funding,
                client: compaction.client.clone(),
            });
        if credit.is_some() {
            agent.count_credit(now);
        }
        self.decide_against(
            &key,
            &agent_id,
            probes,
            Answer {
                started: Some(compaction.started),
                account: compaction.account,
                model: compaction.canonical_model,
                fast: compaction.fast,
                read,
            },
        );
        // No label: a probe still in flight lands orphaned and unsettled, since nothing around a compaction reads as
        // wasted.
        self.discard(&key, &agent_id, Reason::Compacted, None);
        credit
    }
}

/// The prompt a completion reported: its uncached input, cache reads and both cache writes, `None` for an answer that
/// reported none of the four, which leaves the previous prompt in place.
fn reported_prompt(usage: Option<&phoebis_bus::Usage>) -> Option<u64> {
    let usage = usage?;
    let members = [
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_5m_tokens,
        usage.cache_write_1h_tokens,
    ];
    members.iter().any(Option::is_some).then(|| {
        let [input, cache_read, write_5m, write_1h] = members.map(|m| m.unwrap_or(0));
        PinUsage {
            input,
            cache_read,
            write_5m,
            write_1h,
            output: 0,
        }
        .prompt_tokens()
    })
}

impl Inner {
    /// The Go `observeLocked`: evidence from a ready attempt arms the schedule at the entry's expiry, the pool's
    /// lead and this schedule's jitter taken off by `due_at`. A retry already pending stays in force.
    pub(crate) fn observe_usage(&mut self, key: &SessionKey, agent_id: &str, usage: ProbeUsage) {
        let agent = agent_mut(&mut self.sessions, key, agent_id).expect(AGENT_HELD);
        let Some(observation) = agent.current.as_mut() else {
            return;
        };
        if !observation.ready || usage.evidence() == 0 {
            return;
        }
        let previous_ms = agent.ttl_ms;
        let requested_ms = observation
            .dispatched_ms
            .unwrap_or(observation.recipe.ttl_ms);
        let mut ttl_ms = usage.ttl_ms(previous_ms, requested_ms);
        if observation.ttl_ms > 0 && observation.ttl_ms < ttl_ms {
            ttl_ms = observation.ttl_ms;
        }
        // A paused recipe takes nothing from this usage, but the request's credit from a probe sent before the pause
        // still files under the TTL its own write would have taken, so the observation keeps that TTL.
        if agent.paused {
            observation.ttl_ms = ttl_ms;
            return;
        }
        observation.expected = usage.evidence();
        agent.ttl_ms = ttl_ms;
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

impl Engine {
    /// One tick: what is due runs, up to the worker bound. Called by the production ticker every second and by
    /// the tests directly.
    pub async fn step(self: &Arc<Self>) {
        let now = self.now();
        // The six asks behind `router_view` (canonicals, pins, cooldowns, overages, member spends, probe holds) are a
        // round trip apiece, and a tick with no retained recipe pays none of them. `drop_sessions_of_dead_keys` reads
        // the snapshot alone, which `RouterHandle::snapshot` answers without a command.
        let snapshot = self.parts.router.snapshot();
        let retained_models = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            inner.expire(now);
            inner.drop_sessions_of_dead_keys(&snapshot);
            inner.advance_idle_episodes(&snapshot, now);
            // A probe the ceiling wrote off keeps its entry alive for a turn that may still read it, so the write-off
            // waits for the entry's own end, whether or not any recipe is still retained.
            inner.settle_lapsed_pending(now);
            inner.anything_retained().then(|| inner.retained_models())
        };
        let Some(retained_models) = retained_models else {
            return;
        };
        let view = self.router_view(now, retained_models).await;
        // Built once for the whole tick, outside the lock, so every recipe's ceilings read the same prices and no
        // recipe rebuilds a table's rows.
        let revisions = self.parts.pricing.revisions();
        let rows = FactorRows::of(revisions.in_force(now), &self.parts.tables, now);
        let (jobs, lapsed) = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            let mut lapsed: Vec<Skipped> = Vec::new();
            inner.follow_resolutions(&view.canonical);
            let mut due: Vec<(i64, SessionKey, String, Holder)> = Vec::new();
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
                // could never settle it while the companion keeps reporting. A recipe an open compaction holds
                // waits as it stood for the compaction's completion to keep or stop it.
                if agent.current.is_none() || agent.compaction.is_some() {
                    continue;
                }
                let ready = agent
                    .current
                    .as_ref()
                    .is_some_and(|o| o.ready && o.expected > 0);
                // Before any hold can short-circuit: a recipe held under `horizon` never reaches the gate, and the
                // ceilings below read the pool's policy as it stands now, so a horizon raised in the console
                // reopens the hold and an opt-out discards the prompt the hold retained.
                let refreshed = {
                    let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
                    refresh_policy(&view, agent)
                };
                let refreshed = match refreshed {
                    Ok((pool, policy, moved)) => {
                        // Read at every tick, so only a change is written.
                        if moved {
                            inner.touch_agent(&key, &agent_id);
                        }
                        Ok((pool, policy))
                    }
                    Err(Gate::Drop(reason, label)) => {
                        inner.discard(&key, &agent_id, reason, Some(label));
                        continue;
                    }
                    Err(Gate::Hold(reason)) => Err(reason),
                };
                // Ahead of the refresh's hold, so a deadline or a wake that passes while the pool is disabled is
                // recorded when it passes.
                inner.end_passed_park(&key, &agent_id, now);
                let (pool, policy) = match refreshed {
                    Ok(refreshed) => refreshed,
                    Err(reason) => {
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
                };
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
                    let ceilings = ceilings_for(agent, policy, &view, &rows, &self.parts.timing);
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
                let held = gate(
                    &view,
                    &inner.sessions[&key].agents[&agent_id],
                    &key.1,
                    pool,
                    policy,
                    now,
                );
                let holder = match held {
                    Err(Gate::Drop(reason, label)) => {
                        inner.discard(&key, &agent_id, reason, Some(label));
                        continue;
                    }
                    Err(Gate::Hold(reason)) => {
                        inner.hold(&key, &agent_id, reason, now);
                        continue;
                    }
                    Ok(holder) => holder,
                };
                if !ready || inner.sessions[&key].agents[&agent_id].in_flight {
                    continue;
                }
                let Some(at) = inner.sessions[&key].agents[&agent_id].due_at() else {
                    continue;
                };
                if at <= now {
                    due.push((at, key, agent_id, holder));
                }
            }
            due.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
            let mut jobs = Vec::new();
            for (_, key, agent_id, holder) in due {
                if inner.running >= MAX_WORKERS {
                    break;
                }
                let agent = agent_mut(&mut inner.sessions, &key, &agent_id).expect(AGENT_HELD);
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
                    holder,
                    pool_id: agent.pool_id.clone(),
                    canonical_model: agent.canonical_model.clone(),
                    entrypoint: agent.entrypoint.clone(),
                    client_label: observation.client_label.clone(),
                    recipe: observation.recipe.clone(),
                    expected: observation.expected,
                };
                agent.in_flight = true;
                agent.hold_counted = false;
                agent.probes += 1;
                agent.count_probe(now);
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

    /// Quiesces, waits for the probes the quiesce canceled, saves, and only then clears: the last save carries
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
    pub cooled: BTreeMap<(String, CooldownScope), String>,
    /// The accounts whose usage credits block the fast lane: the gate holds a fast probe pinned there.
    pub credits: BTreeSet<String>,
    pub overage: BTreeSet<String>,
    /// `(key, session, agent, canonical model)` to the pinned account's id and the incarnation the pin was made on:
    /// the holder a probe would actually run on, so a sibling's pin never overwrites an agent's own.
    pub pins: BTreeMap<(String, String, String, String), (String, u64)>,
    /// Each pool's keepalive policy, parsed once per tick rather than once per recipe.
    pub policies: HashMap<String, KeepalivePolicy>,
    pub snapshot: Arc<RegistrySnapshot>,
    /// The usage-based seats at their spend cap: the gate holds a probe pinned there.
    pub spent: BTreeSet<String>,
}

/// Every pool's keepalive policy in `snapshot`, by pool id.
pub(crate) fn pool_policies(snapshot: &RegistrySnapshot) -> HashMap<String, KeepalivePolicy> {
    snapshot
        .pools()
        .map(|pool| {
            (
                pool.id.clone(),
                KeepalivePolicy::from_json(&pool.keepalive_json),
            )
        })
        .collect()
}

/// What the gate decided for a due recipe: wait with a reason, or drop it with the label its probes settle under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Drop(Reason, Wasted),
    Hold(Reason),
}

impl Engine {
    /// Six asks of the router task per tick, whatever the number of recipes: the scheduler must not queue one
    /// command per agent in front of selection.
    pub(crate) async fn router_view(&self, now: i64, retained_models: Vec<String>) -> RouterView {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.router_view_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let snapshot = self.parts.router.snapshot();
        let canonical = self.dated_names(retained_models).await;
        let pins = self
            .parts
            .router
            .pins()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                (
                    (p.key_id, p.session_id, p.agent_id, p.model),
                    (p.account_id, p.incarnation),
                )
            })
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
        let holds = self.parts.router.probe_holds().await.unwrap_or_default();
        RouterView {
            budgets,
            canonical,
            cooled,
            credits: holds.credits.into_iter().collect(),
            overage,
            pins,
            policies: pool_policies(&snapshot),
            snapshot,
            spent: holds.spent.into_iter().collect(),
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

/// The key's opt-out drops the recipe, a disabled pool holds it, and the pool's current id lands on the agent. Answers
/// the key's pool and its current policy, which the ceilings and the gate read, so a horizon raised in the console
/// reaches a held recipe at the next tick and an opt-out discards a held prompt rather than leaving it retained, and
/// whether the agent's pool moved.
pub(crate) fn refresh_policy<'v>(
    view: &'v RouterView,
    agent: &mut Agent,
) -> Result<(&'v PoolRow, &'v KeepalivePolicy, bool), Gate> {
    // The operator's opt-out reaches an idle session here, not only at its next request: the recipe is dropped
    // rather than held, because nothing on that key may be probed again until its keepalive permission is back.
    let Some(key) = view
        .snapshot
        .key(&agent.key_id)
        .filter(|k| k.companion_permissions.keepalive)
    else {
        return Err(Gate::Drop(Reason::KeyOptedOut, Wasted::Released));
    };
    // The view's map is read in the same lookup, so a pool the map lacks holds as a disabled one does.
    let Some((pool, policy)) = view
        .snapshot
        .pool(&key.pool_id)
        .filter(|p| p.enabled)
        .zip(view.policies.get(&key.pool_id))
    else {
        return Err(Gate::Hold(Reason::PoolDisabled));
    };
    // The pool's policy may have changed since admission, and an admin may have moved the key to another pool
    // without a request following. The ceilings, the budget and the probe's own delta all read the pool the key
    // belongs to now.
    let moved = agent.pool_id != key.pool_id;
    agent.pool_id = key.pool_id.clone();
    Ok((pool, policy, moved))
}

/// The ceilings of one agent at a tick: its policy, the factors of its model under its account's funding (dollars
/// at the fast cells for a fast recipe, dollars for an account the view lacks) from the tick's rows, its TTL and the
/// engine's timing. Tables that give no break-even (no priced row, or a free read) fall back to the shipped price
/// table's tightest row at the recipe's speed, so a ceiling is never unbounded. A timing whose lead is the whole TTL
/// leaves no probe interval, and nothing is probed.
pub(crate) fn ceilings_for(
    agent: &Agent,
    policy: &KeepalivePolicy,
    view: &RouterView,
    rows: &FactorRows,
    timing: &KeepaliveTiming,
) -> Ceilings {
    let fast = agent.current.as_ref().is_some_and(|o| o.recipe.fast);
    let funding = agent
        .account
        .as_deref()
        .and_then(|id| view.snapshot.account(id))
        .map_or(Funding::ApiKey, |account| {
            funding_of(
                Some(account.kind),
                view.snapshot.usage_based(&account.id),
                Some(view.overage.contains(&account.id)),
                Speed::Standard,
            )
        });
    resolve_or_shipped(
        policy,
        rows.factors(
            funding,
            &agent.canonical_model,
            fast,
            agent.current.as_ref().map_or(0, |o| o.expected),
        ),
        floor_ttl_ms(agent.ttl_ms),
        timing,
        fast,
        rows,
    )
    .unwrap_or(Ceilings {
        idle_ms: 0,
        parked_ms: 0,
        horizon_ms: 0,
        break_even_ms: 0,
        idle_share: 0.0,
    })
}

/// The spec's "Scheduling against router state" row, in its order, after `refresh_policy` answered `pool`. `Err`
/// names why the due probe waits, or why the recipe goes. `Ok` lets the probe run on the holder it names, the recipe's
/// holder: the account id with the incarnation the recipe was last answered on. The pinned account's subscription term
/// holds the probe under `ineligible_credential` once it ended, and while the probe's due time plus its interval (the
/// TTL less the lead) passes the end, since the cache that probe writes dies at the end. The end is `term::end_of`'s,
/// the one the router's wind-down reads, so the last interval before a trial end still ahead holds too and probing
/// resumes once the trial end passes.
pub(crate) fn gate(
    view: &RouterView,
    agent: &Agent,
    session_id: &str,
    pool: &PoolRow,
    policy: &KeepalivePolicy,
    now: i64,
) -> Result<Holder, Gate> {
    let key_id = agent.key_id.clone();
    // A probe keeps the session on the account it warmed only while the pin outlives the probe's due time:
    // the recipe's own TTL less the lead, so a five-minute recipe needs about four minutes of pin and a
    // one-hour one about fifty-four. The save-time floor of 300 s stays the one hard refusal.
    let due_after_ms = floor_ttl_ms(agent.ttl_ms) - agent.timing.lead_ms(agent.ttl_ms);
    if seconds_ms(pool.affinity_ttl_seconds) < due_after_ms {
        return Err(Gate::Hold(Reason::AffinityTooShort));
    }
    if !policy.enabled {
        return Err(Gate::Hold(Reason::KeepaliveOff));
    }
    // A recipe that names no incarnation of its holder cannot tell a re-paired account from the one that answered it,
    // so it waits for the agent's next request, which arms it.
    let Some(incarnation) = agent.account_incarnation else {
        return Err(Gate::Hold(Reason::PinMoved));
    };
    let pinned = view.pins.get(&(
        key_id.clone(),
        session_id.to_string(),
        agent.id.clone(),
        agent.canonical_model.clone(),
    ));
    // The probe replays the prompt on the holder that last answered the recipe. A pin that names another holder has
    // moved since, and a probe there would write the whole prompt cold, so the recipe waits for the next request that
    // arms it on the pin's account.
    if let Some((pinned, pinned_incarnation)) = pinned {
        if agent.account.as_deref() != Some(pinned.as_str()) || incarnation != *pinned_incarnation {
            return Err(Gate::Hold(Reason::PinMoved));
        }
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
    // A hold rather than a pause, read live on every poll like the budget below: an operator who turns the cancel
    // off, or a profile read that clears the plan-ended flag, releases the probe on the next tick.
    if let Some(term) = view.snapshot.term(&account.id) {
        let last_interval = agent
            .due_at()
            .zip(phoebis_routing::term::end_of(term, now))
            .is_some_and(|(due, end)| due + due_after_ms > end);
        if last_interval || phoebis_routing::term::ended(term, now) {
            return Err(Gate::Hold(Reason::IneligibleCredential));
        }
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
    // next tick, and a real request that moves the pin clears it, since it arms the recipe on the pin's account.
    if view
        .budgets
        .contains(&(pool.id.clone(), account.id.clone()))
    {
        return Err(Gate::Hold(Reason::Budget));
    }
    // The selector's own two tests on the pin, read live like the holds above, so such a probe writes no rejected
    // request row at each rung of the ladder. The spend funds every lane, and the credits the fast one alone.
    if view.spent.contains(&account.id) {
        return Err(Gate::Hold(Reason::Spent));
    }
    if fast && view.credits.contains(&account.id) {
        return Err(Gate::Hold(Reason::Credits));
    }
    Ok(Holder {
        account_id: account.id.clone(),
        incarnation,
    })
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
    fn the_longest_cache_ttl_reads_both_sets_and_their_model_overrides_while_the_policy_is_off() {
        let longest =
            |raw: serde_json::Value| longest_cache_ttl_ms(&CacheTtlPolicy::parse(&raw).unwrap());
        let five = serde_json::json!({"main": "5m", "subagent": "5m"});
        let with_model = |main: &str, subagent: &str| {
            serde_json::json!({
                "main": "5m",
                "models": [{"main": main, "model": "claude-opus-4-1", "subagent": subagent}],
                "subagent": "5m",
            })
        };
        assert_eq!(longest(serde_json::json!({})), ONE_HOUR_MS);
        assert_eq!(
            longest(serde_json::json!({"metered": five, "plan": five})),
            FIVE_MINUTES_MS
        );
        for (name, policy) in [
            (
                "a metered subagent",
                serde_json::json!({"metered": {"main": "5m", "subagent": "1h"}, "plan": five}),
            ),
            (
                "a plan main",
                serde_json::json!({"metered": five, "plan": {"main": "1h", "subagent": "5m"}}),
            ),
            (
                "a metered model's main",
                serde_json::json!({"metered": with_model("1h", "5m"), "plan": five}),
            ),
            (
                "a plan model's subagent",
                serde_json::json!({"metered": five, "plan": with_model("5m", "1h")}),
            ),
        ] {
            assert_eq!(longest(policy), ONE_HOUR_MS, "{name}");
        }
    }

    #[test]
    fn a_refreshing_class_rearms_whatever_the_chain_says() {
        assert_eq!(
            guard(Some("subagent"), None, &[], Some(("s", "t")), false),
            Guard::Rearm
        );
    }

    #[test]
    fn a_compaction_answers_its_own_verdict_whatever_the_chain_says() {
        let continuing = ["t".to_string()];
        let fresh = ["fresh".to_string()];
        for (static_hash, boundaries, recipe, model_matches) in [
            (None, &[][..], Some(("s", "t")), false),
            (None, &[][..], None, false),
            (Some("s"), &continuing[..], Some(("s", "t")), true),
            (Some("s"), &fresh[..], Some(("s", "t")), true),
        ] {
            assert_eq!(
                guard(
                    Some("compaction"),
                    static_hash,
                    boundaries,
                    recipe,
                    model_matches
                ),
                Guard::Compaction,
                "{static_hash:?} {boundaries:?} {recipe:?}"
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

    /// The class decides after the kind: a usage-based seat's probe settles in dollars whatever the overage flag, as an
    /// API-key member's does, and a plan account's in its window share or, in overage, in dollars.
    #[test]
    fn a_usage_based_seat_funds_its_probes_in_dollars_whatever_the_overage_flag() {
        for in_overage in [false, true] {
            assert_eq!(
                (
                    funding_of(
                        Some(AccountKind::Oauth),
                        true,
                        Some(in_overage),
                        Speed::Standard
                    ),
                    funding_of(
                        Some(AccountKind::ApiKey),
                        false,
                        Some(in_overage),
                        Speed::Standard
                    )
                ),
                (Funding::UsageBased, Funding::ApiKey),
                "overage flag {in_overage}"
            );
        }
        assert_eq!(
            (
                funding_of(
                    Some(AccountKind::Oauth),
                    false,
                    Some(false),
                    Speed::Standard
                ),
                funding_of(Some(AccountKind::Oauth), false, Some(true), Speed::Standard)
            ),
            (Funding::Plan, Funding::Overage)
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
        let account = |id: &str, kind| phoebis_store::AccountRow::for_tests(id, kind);
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
                companion_permissions: phoebis_store::CompanionPermissions::default(),
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
                feature_accounts: phoebis_store::KeyFeatureAccounts::default(),
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
        let snapshot = Arc::new(RegistrySnapshot::from_load(load, [0; 32], 0, 1));
        RouterView {
            budgets: BTreeSet::new(),
            canonical: HashMap::new(),
            cooled: BTreeMap::new(),
            credits: BTreeSet::new(),
            overage: if o_in_overage {
                BTreeSet::from(["o".to_string()])
            } else {
                BTreeSet::new()
            },
            pins: BTreeMap::new(),
            policies: pool_policies(&snapshot),
            snapshot,
            spent: BTreeSet::new(),
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
            received_at: 0,
            recipe,
            ready: true,
            expected: 2_000,
            ttl_ms: 0,
            prior: None,
            poisoned: false,
            dispatched_ms: None,
            funding: AttemptFunding::default(),
            client_label: None,
        }
    }

    /// The gate's answer for `main` holding a recipe of that speed answered on `account`, with no pin in the view.
    fn gated_at(view: &RouterView, account: &str, fast: bool) -> Option<Gate> {
        let mut agent = Agent::new("main", "k", 1, 0, KeepaliveTiming::default());
        agent.account = Some(account.into());
        agent.account_incarnation = Some(1);
        agent.canonical_model = "claude-opus-5-5".into();
        agent.current = Some(observation(fast));
        refresh_policy(view, &mut agent)
            .and_then(|(pool, policy, _)| gate(view, &agent, "s", pool, policy, 0))
            .err()
    }

    /// A recipe's ceilings read the price set its prompt selects, its length the tokens the cache evidence counted: the
    /// fixture's body is under a hundred bytes, and its 2,000 tokens put it over the threshold.
    #[test]
    fn a_recipes_ceilings_read_the_price_set_its_prompt_tokens_select() {
        let pricing = phoebis_ledger::PriceRevisions::from_yaml(
            r#"
revisions:
  - effective: 2026-01-01
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
        let rows = FactorRows::of(pricing.latest(), &PlanTables::shipped(), 0);
        let timing = KeepaliveTiming::default();
        let mut agent = Agent::new("main", "k", 1, 0, timing);
        agent.account = Some("a".into());
        agent.canonical_model = "claude-opus-5-5".into();
        agent.current = Some(observation(false));
        let policy = KeepalivePolicy::default();
        let break_even = |agent: &Agent| {
            ceilings_for(agent, &policy, &pool_view(false), &rows, &timing).break_even_ms
        };
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
            cooled: BTreeMap::from([(
                ("o".to_string(), CooldownScope::parse(scope).unwrap()),
                window.to_string(),
            )]),
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
        // Each row's pair is whether the row holds a standard recipe and a fast one.
        let rows = [
            ("account", "five_hour", (true, false)),
            ("account", "seven_day", (true, false)),
            ("account", "seven_day_fable", (true, false)),
            ("account", "retry_after", (true, true)),
            ("fast", "fast", (false, true)),
            ("model:claude-opus-5-5", "retry_after", (true, true)),
            ("model:claude-sonnet-5", "retry_after", (false, false)),
        ];
        for (scope, window, holds) in rows {
            let view = RouterView {
                cooled: BTreeMap::from([(
                    ("o".to_string(), CooldownScope::parse(scope).unwrap()),
                    window.to_string(),
                )]),
                ..pool_view(false)
            };
            assert_eq!(
                (
                    gated_at(&view, "o", false) == Some(Gate::Hold(Reason::Cooled)),
                    gated_at(&view, "o", true) == Some(Gate::Hold(Reason::Cooled))
                ),
                holds,
                "{scope} at {window}"
            );
        }
    }

    /// `main` of session `s` holding a standard recipe last answered on `account` under `incarnation`.
    fn holding(account: &str, incarnation: Option<u64>) -> Agent {
        let mut agent = Agent::new("main", "k", 1, 0, KeepaliveTiming::default());
        agent.account = Some(account.into());
        agent.account_incarnation = incarnation;
        agent.canonical_model = "claude-opus-5-5".into();
        agent.current = Some(observation(false));
        agent
    }

    /// `pool_view` with `main`'s pin on `account` under `incarnation`.
    fn pinned_on(account: &str, incarnation: u64) -> RouterView {
        RouterView {
            pins: BTreeMap::from([(
                (
                    "k".to_string(),
                    "s".to_string(),
                    "main".to_string(),
                    "claude-opus-5-5".to_string(),
                ),
                (account.to_string(), incarnation),
            )]),
            ..pool_view(false)
        }
    }

    /// The gate's whole answer for `agent` under `view`, after the policy's refresh.
    fn gate_of(view: &RouterView, agent: &mut Agent) -> Result<Holder, Gate> {
        refresh_policy(view, agent)
            .and_then(|(pool, policy, _)| gate(view, agent, "s", pool, policy, 0))
    }

    fn holder(account: &str, incarnation: u64) -> Holder {
        Holder {
            account_id: account.into(),
            incarnation,
        }
    }

    #[test]
    fn a_due_probe_holds_while_its_pin_stands_on_another_holder() {
        assert_eq!(
            gate_of(&pinned_on("o", 1), &mut holding("a", Some(1))),
            Err(Gate::Hold(Reason::PinMoved)),
            "a pin on another account holds the recipe rather than writing its prompt cold there"
        );
        assert_eq!(
            gate_of(&pinned_on("a", 1), &mut holding("a", Some(1))),
            Ok(holder("a", 1)),
            "a pin on the recipe's own holder lets the probe run there"
        );
        assert_eq!(
            gate_of(&pinned_on("a", 2), &mut holding("a", Some(1))),
            Err(Gate::Hold(Reason::PinMoved)),
            "a pin on the same id paired again is another holder"
        );
    }

    #[test]
    fn a_recipe_with_no_holder_incarnation_holds_with_no_pin_in_the_view() {
        assert_eq!(
            gate_of(&pool_view(false), &mut holding("a", None)),
            Err(Gate::Hold(Reason::PinMoved))
        );
    }

    #[test]
    fn a_recipe_put_back_after_a_failed_request_gates_on_its_own_holder() {
        let key: SessionKey = ("k".into(), "s".into());
        let mut inner = Inner::empty(KeepaliveTiming::default(), KeepaliveLimits::default(), 0);
        let mut armed = holding("a", Some(1));
        armed.expires = Some(300_000);
        inner.sessions.insert(
            key.clone(),
            Session {
                key_generation: 0,
                instance: "i".into(),
                sequence: 0,
                lease: i64::MAX,
                attended: true,
                ended: false,
                owner_id: None,
                last_report_at: 0,
                last_real_at: Some(0),
                slotted: true,
                restored_until: None,
                owner_mismatches: 0,
                facts: SessionFacts::default(),
                agents: BTreeMap::from([("main".to_string(), armed)]),
            },
        );
        inner.set_aside(&key, "main", 1_000);
        // The arrival's selection moved the agent onto the holder of the request that then failed.
        let agent = agent_mut(&mut inner.sessions, &key, "main").expect(AGENT_HELD);
        agent.account = Some("o".into());
        agent.account_incarnation = Some(3);
        let aside = inner
            .take_aside(&key, "main")
            .expect("the arrival set the recipe aside");
        assert!(inner.rearm_aside(&key, "main", aside, 2_000));
        let agent = agent_mut(&mut inner.sessions, &key, "main").expect(AGENT_HELD);
        assert_eq!(gate_of(&pinned_on("a", 1), agent), Ok(holder("a", 1)));
    }

    #[test]
    fn a_probe_pinned_on_a_seat_at_its_spend_cap_holds_as_spent() {
        let spent = RouterView {
            spent: BTreeSet::from(["a".to_string()]),
            ..pool_view(false)
        };
        assert_eq!(
            (
                gated_at(&spent, "a", false),
                gated_at(&pool_view(false), "a", false)
            ),
            (Some(Gate::Hold(Reason::Spent)), None)
        );
    }

    #[test]
    fn a_fast_recipe_holds_on_blocked_credits_and_a_standard_one_passes() {
        let blocked = RouterView {
            credits: BTreeSet::from(["a".to_string()]),
            ..pool_view(false)
        };
        assert_eq!(
            (
                gated_at(&blocked, "a", true),
                gated_at(&blocked, "a", false)
            ),
            (Some(Gate::Hold(Reason::Credits)), None)
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
