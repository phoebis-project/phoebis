//! One selection: the spec's seven steps over the snapshot and the routing state.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use phoebis_store::{
    AccountKind, AccountRow, AccountState, AffinityMode, BudgetPeriod, CooldownRow, OveragePolicy,
    PinBurn, PinKey, PinRow, PoolMember, PoolRow, SessionPlacement, ShedPin, Strategy,
    SubagentPlacement, WarmSessions,
};
use rand::{Rng, RngExt};

use crate::affinity::{AffinityEntry, AffinityMap, RungWalk, Visited};
use crate::record::{
    key_fact, settings_of, BudgetFacts, By, CandidateFacts, ChainFacts, CooldownFacts,
    CreditsFacts, DecisionRecord, DecisionSummary, ExhaustionFacts, Fact, FactEntity, FactValue,
    HoldOutcome, LaneFacts, ObstacleFacts, Outcome, OverageFacts, PaceFacts,
    PinFacts, PinKeyFacts, PinShed, PinWritten, PrefixEntry, PrefixLookup, RefusalFacts,
    RequestFacts, RequestFlags, Score, ShedPinFacts, ShedRoundFacts, SummaryFact, TermFacts,
    WalkStep, WarmthFacts, WindowFacts, RECORD_VERSION, WALK_STEPS,
};
use crate::state::{
    budget_reached, shed_marks, warm, BurnUnit, CreditsState, MemberSpend, PinRef, RoutingState,
    StateWrite, Tier, WindowReading, FAST_INPUT, FAST_OUTPUT, FIVE_HOUR, SEVEN_DAY,
    SEVEN_DAY_FABLE, WEEK_MS,
};
use crate::windows::window_for;
use crate::{term, Candidate, KeyRefusal, RegistrySnapshot, ResolveError};

/// Who sent the request: `claude-code/*` is Claude Code, everything else is not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientClass {
    ClaudeCode,
    #[default]
    Other,
}

impl ClientClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientClass::ClaudeCode => "claude_code",
            ClientClass::Other => "other",
        }
    }

    pub fn of_label(label: &str) -> ClientClass {
        if label.starts_with("claude-code/") {
            ClientClass::ClaudeCode
        } else {
            ClientClass::Other
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectRequest {
    /// The request this decision belongs to and the wire attempt it dispatches, numbered from 1 as the bus and the
    /// attempt rows number it (the relay's counter at the selection plus one, since `dispatch_to` increments it
    /// before publishing). `seq` counts the selections the relay asked for the request from 1, so a hold and the
    /// placement after it are two decisions of one attempt, and a retry in place is one decision over two attempts.
    pub request_id: String,
    pub attempt: u32,
    pub seq: u32,
    pub key_id: String,
    /// The authenticated key's `created_at` at admission. The id is reusable, so the lifetime is the identity: a
    /// stale-revision re-authentication that finds another generation under the id refuses the request.
    pub key_generation: i64,
    /// The identity's agent: `main`, or a subagent's own id, so a pin is held per agent of a session.
    pub agent_id: String,
    pub session_id: Option<String>,
    pub canonical_model: String,
    /// The client's spelling with the `[1m]` suffix removed and ASCII lowercased, the relay's own folded id, so
    /// the stale-revision re-resolve keeps the client's spelling rather than the canonical id, also when the
    /// admission resolved no candidate to copy it from.
    pub folded_model: String,
    pub candidates: Vec<Candidate>,
    pub portable: bool,
    pub attempted: Vec<String>,
    pub now: i64,
    pub fallback_ms: i64,
    pub revision: u64,
    /// A keepalive probe's replay of the recipe, from the relay trace's `Kind::Probe`; its headers and body
    /// are the re-arming request's, so it carries no warmth verdict of its own.
    pub probe: bool,
    /// A `count_tokens` request, from the relay trace's `Kind::CountTokens`: it writes no cache, so it carries
    /// no warmth verdict either, whatever its messages continue.
    pub count: bool,
    /// `x-claude-code-request-class` as sent, `None` without the header.
    pub request_class: Option<String>,
    /// `x-claude-code-context-compacted` was present.
    pub context_compacted: bool,
    /// The static chain's last boundary and every boundary of the message chain, lowercase hex; empty when
    /// the body carried no chain.
    pub static_hash: Option<String>,
    /// Shared with the attempts before and after and with the answer's affinity report, never copied per attempt.
    pub boundaries: Arc<[String]>,
    pub client: ClientClass,
    /// Every static boundary in chain order; `static_hash` stays the last one.
    pub static_boundaries: Arc<[String]>,
    /// The body carries a `cache_control` marker, or the TTL policy will add one: the cache gate.
    pub marker: bool,
    /// `x-claude-code-parent-agent-id` as sent; `with_main` reads it, `main` when absent.
    pub parent_agent_id: Option<String>,
    /// The holds this request already waited through, oldest first: the relay carries each `Held` answer's
    /// facts into the next selection, so the attempt that follows a hold names it.
    pub holds: Vec<HoldFacts>,
    /// The cache lane the request writes to, as the relay composed it; a pin or an entry of another lane is cold.
    pub lane: String,
    /// The body carried `speed: "fast"`: fast eligibility applies.
    pub fast: bool,
}

/// The cache lane key, `<model>|<speed>|<thinking>|<effort>`: the one spelling the relay composes and every pin,
/// affinity entry and request compares, so no reader spells it twice.
pub fn lane_key(model: &str, speed: &str, thinking: &str, effort: &str) -> String {
    format!("{model}|{speed}|{thinking}|{effort}")
}

/// Whether a pin or an entry stored on `stored` holds the cache a request on `lane` writes to. A row written
/// before rows carried a lane stores none and adopts the request's, since it never recorded another cache.
pub(crate) fn same_lane(stored: &str, lane: &str) -> bool {
    stored.is_empty() || stored == lane
}

/// Whether `lane` is a fast one: its speed part, the second of the three after the model, reads `fast`.
fn lane_is_fast(lane: &str) -> bool {
    lane.rsplit('|').nth(2) == Some("fast")
}

/// Where a fresh placement's account came from, for the trace and the console's hit-or-miss reading.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Placement {
    /// `content` (the deepest message boundary), `static` (the deepest static boundary), `with_main` (the
    /// parent agent's pin, or another agent's pin of the session when every account was at its cap).
    pub rung: &'static str,
    /// The matched boundary's depth (index plus one); 0 for `with_main`.
    pub depth: u32,
    /// The prefix size the entry promised, in tokens; what a hit should read up to. 0 for `with_main`, which
    /// names no entry.
    pub prefix_tokens: u64,
}

/// What a hold decided, carried across the wait by the relay.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HoldFacts {
    /// The account the request waited on.
    pub account_id: String,
    /// The cohort's account the hold was placed by (a subagent's deepest live static entry); `None` for a hold
    /// on the strategy's choice.
    pub cohort: Option<String>,
    /// How long the relay waited on this hold, whether a slot freed or the bound ran out.
    pub held_ms: u64,
    /// The wire attempt that waited: the relay keeps every hold across a failover, and only the placement of this
    /// attempt follows the wait.
    pub attempt: u32,
}

/// The holds before this attempt, as the trace records them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CohortHold {
    pub waited_on: Vec<String>,
    pub cohort: Option<String>,
    /// The holds' waits summed: the time the request spent waiting for a slot.
    pub held_ms: u64,
}

/// The summary of the holds a request waited through, `None` for none: the accounts waited on in order, the last
/// hold's cohort and the waits summed. The selector's trace and the relay's completion both read it, so the
/// attempt's entry and the request row never disagree.
pub fn cohort_hold(holds: &[HoldFacts]) -> Option<CohortHold> {
    let last = holds.last()?;
    Some(CohortHold {
        waited_on: holds.iter().map(|h| h.account_id.clone()).collect(),
        cohort: last.cohort.clone(),
        held_ms: holds.iter().map(|h| h.held_ms).sum(),
    })
}

/// Why a pin moved, in the order the rules run: the first rule with a destination moves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrateReason {
    /// A cold pin left an account winding down toward its end.
    Drain,
    /// A subscription account in overage handed its session back to plan quota.
    Overage,
    /// A pin on an api_key account returned to plan quota once cold or past the dwell.
    ReturnToPlan,
    /// A shedding round marked the pin.
    Shed,
    /// The pinned account's window for the request's model is spent while a sibling holds allowance in it.
    Spent,
}

impl MigrateReason {
    pub fn as_str(self) -> &'static str {
        match self {
            MigrateReason::Drain => "drain",
            MigrateReason::Overage => "overage",
            MigrateReason::ReturnToPlan => "return_to_plan",
            MigrateReason::Shed => "shed",
            MigrateReason::Spent => "spent",
        }
    }
}

/// Which classes refresh a pin's warmth and re-arm a recipe; every other class is auxiliary.
pub const REFRESHING_CLASSES: &[&str] = &["compaction", "main", "subagent"];

/// What the incoming request says about the pin's cache: a refreshing turn, a cold one (compacted or
/// restarted), or nothing (a foreign request, or an auxiliary class).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warmth {
    Cold,
    Refresh,
    Untouched,
}

impl Warmth {
    pub fn as_str(self) -> &'static str {
        match self {
            Warmth::Cold => "cold",
            Warmth::Refresh => "refresh",
            Warmth::Untouched => "untouched",
        }
    }
}

/// The branch of `warmth_of` that decided the verdict, one name per return.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarmthCause {
    Probe,
    Count,
    Compacted,
    LaneChanged,
    ClassRefreshes,
    ClassAuxiliary,
    Chainless,
    ContinuesPin,
    StaticRestart,
    Foreign,
}

impl WarmthCause {
    pub fn as_str(self) -> &'static str {
        match self {
            WarmthCause::Probe => "probe",
            WarmthCause::Count => "count",
            WarmthCause::Compacted => "compacted",
            WarmthCause::LaneChanged => "lane_changed",
            WarmthCause::ClassRefreshes => "class_refreshes",
            WarmthCause::ClassAuxiliary => "class_auxiliary",
            WarmthCause::Chainless => "chainless",
            WarmthCause::ContinuesPin => "continues_pin",
            WarmthCause::StaticRestart => "static_restart",
            WarmthCause::Foreign => "foreign",
        }
    }
}

/// The verdict of `request` on the pin it arrives for. The compacted flag is cold whatever else the request says; a
/// request on another cache lane than the pin's last is cold whatever its messages continue, unless it leaves the
/// pin alone (a class that does not refresh, or a foreign request); the class decides when present; without it, a
/// request whose boundaries contain the pin's tail or name an entry the pin's key and generation wrote on the pin's
/// account and incarnation continues the conversation (so where one session holds several conversations, a
/// continuation of any of them refreshes the pin), one that matches the static prefix without such a boundary
/// restarted it, and one matching neither is foreign. A body
/// without a chain refreshes, the fallback for a request that carries no fact. A keepalive probe replays the
/// recipe's headers and body verbatim, the re-arming request's rather than its own, so it is judged by none of
/// this: its renewal report stamps the refresh instead. A token count is judged by none of it either, since it
/// creates no cache entry to be warm about. The cause beside the verdict names the branch that decided it.
pub fn warmth_of(
    current: &PinRow,
    request: &SelectRequest,
    affinity: &AffinityMap,
) -> (Warmth, WarmthCause) {
    if request.probe {
        return (Warmth::Untouched, WarmthCause::Probe);
    }
    if request.count {
        return (Warmth::Untouched, WarmthCause::Count);
    }
    if request.context_compacted {
        return (Warmth::Cold, WarmthCause::Compacted);
    }
    // Another lane is another cache: a refreshing turn on it reads the pin cold, whatever its boundaries. A request
    // that leaves the pin alone still does, since it never touched the conversation's prefix.
    let lane_changed = !same_lane(&current.lane, &request.lane);
    let refreshed = |cause: WarmthCause| {
        if lane_changed {
            (Warmth::Cold, WarmthCause::LaneChanged)
        } else {
            (Warmth::Refresh, cause)
        }
    };
    if let Some(class) = request.request_class.as_deref() {
        return if REFRESHING_CLASSES.contains(&class) {
            refreshed(WarmthCause::ClassRefreshes)
        } else {
            (Warmth::Untouched, WarmthCause::ClassAuxiliary)
        };
    }
    if request.static_hash.is_none() && request.boundaries.is_empty() {
        return refreshed(WarmthCause::Chainless);
    }
    // The pin's tail stays an anchor beside the map: the affinity report lands when the previous answer ends,
    // and a quick continuation can select before it does.
    let on_pin = |entry: &AffinityEntry| {
        entry.account_id == current.account_id
            && entry.incarnation == current.incarnation
            && entry.key_id == current.key_id
            && entry.key_generation == current.key_generation
    };
    if request.boundaries.iter().any(|b| {
        Some(b) == current.tail_hash.as_ref()
            || affinity
                .entry(&request.canonical_model, b)
                .is_some_and(on_pin)
    }) {
        return refreshed(WarmthCause::ContinuesPin);
    }
    if request.static_hash.is_some() && request.static_hash == current.static_hash {
        return (Warmth::Cold, WarmthCause::StaticRestart);
    }
    (Warmth::Untouched, WarmthCause::Foreign)
}

/// The pin's warmth on this request: `Refresh` is warm, `Cold` is cold, `Untouched` reads `warm(current, now)`.
pub fn effective_warm(current: &PinRow, request: &SelectRequest, affinity: &AffinityMap) -> bool {
    warm_on(
        current,
        warmth_of(current, request, affinity).0,
        request.now,
    )
}

fn warm_on(current: &PinRow, verdict: Warmth, now: i64) -> bool {
    match verdict {
        Warmth::Cold => false,
        Warmth::Refresh => true,
        Warmth::Untouched => warm(current, now),
    }
}

/// The request's two hashes, for a pin it refreshed, colded or created: the static prefix and the last message
/// boundary, which the next continuation's boundaries contain.
fn stamp_hashes(pin: &mut PinRow, request: &SelectRequest) {
    pin.static_hash = request.static_hash.clone();
    pin.tail_hash = request.boundaries.last().cloned();
}

/// The hysteresis band under the shedding margin (`shed_ahead_of_pace`): a subscription destination counts only below
/// `margin - MIGRATE_BAND`; an api_key one reads its bucket against 1.0.
pub const MIGRATE_BAND: f64 = 0.05;

/// Why a selection landed where it did, for the attempt row and the request drawer. `Serialize` here, so the
/// relay can put the trace on the bus as JSON without this crate's callers needing to depend on the ledger.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct SelectionTrace {
    pub strategy: &'static str,
    /// The tier the choice was made in (group, priority, overage bit) as a three-element array; `None` for a
    /// renewed pin or a fresh placement, since neither ran the ranking `choose` does.
    pub tier: Option<Tier>,
    pub renewed: bool,
    pub moved: Option<&'static str>,
    /// Every candidate the choice passed over.
    pub skipped: Vec<Skipped>,
    /// Where a fresh placement's account came from; `None` for a renewal or a choice the strategy made.
    pub placement: Option<Placement>,
    /// The holds this attempt's request already waited through; `None` for one that never held.
    pub cohort_hold: Option<CohortHold>,
}

/// One candidate a choice passed over, and why; serializes as `{"account_id": ..., "obstacle": ...}` so the
/// console decodes a named object, not a tuple.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Skipped {
    pub account_id: String,
    /// A step-2 obstacle (`attempted`, `budget`, `budget_unknown`, `cooldown`, `credits`, `operator`, `overage`,
    /// `subscription_ended`), or a partition's name: `above_ceiling`, `at_cap`, `floor`, `spent`, `winding_down`,
    /// or `ranked` for an eligible sibling the strategy did not pick.
    pub obstacle: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    /// The row the selection was made against, so what a request dispatches with is the endpoint and the
    /// credential one snapshot held together rather than two lookups taken moments apart.
    pub account: AccountRow,
    pub upstream_model: String,
    pub pin: Option<PinRef>,
    pub migrated: Option<MigrateReason>,
    /// The account's overage state at this selection, so an edit that prices a write reads the same fact the
    /// selector did.
    pub in_overage: bool,
    pub trace: SelectionTrace,
    /// The pool the candidates were resolved under: a selection re-resolved on a newer revision may name another
    /// pool than the request was admitted under, which the relay refuses rather than dispatch under one pool's
    /// policy on another's member.
    pub pool_id: String,
}

/// The reason a rejection cooldown row carries; the verdict writes it and the report below reads it.
pub const MODEL_REJECTED_REASON: &str = "model_rejected";

/// The reason a fast request's 429 that names an overage-disabled reason cools the fast lane under, on the
/// `fast_credits` row; the row's `window` is that reason, which the local answer hands back to the client.
pub const FAST_CREDITS_REASON: &str = "fast_credits";

/// The reason a fast request's 429 without that reason cools the fast lane under, on the `fast` row, apart from the
/// credits row so each lifts on its own evidence; the row's `window` is `fast`, or `retry_after` when only
/// `Retry-After` timed it, so the in-place retry treats it as any timed throttle.
pub const FAST_RATE_LIMIT_REASON: &str = "fast_rate_limit";

/// The exhaustion window and the outcome stamp of a probe answered on its pinned member's spend budget: the
/// selector names it, the relay stamps and claims it, the keepalive reads it.
pub const BUDGET_WINDOW: &str = "budget";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exhaustion {
    /// No candidate is eligible, and every cooldown in the way is a rejection of the request's model.
    ModelRejected,
    Timed {
        deadline: i64,
        window: String,
    },
    Untimed,
    /// Every fast-eligible candidate is out and at least one holds only a `fast_rate_limit` row: the earliest
    /// deadline.
    FastLane {
        deadline: i64,
    },
    /// Every candidate the pool could use is credits-blocked: the pinned account's reason, else the first in order.
    FastCredits {
        reason: String,
    },
    /// A probe whose pinned member cannot serve it for an obstacle no wait clears (`attempted` while a sibling
    /// could take the probe, `budget_unknown` for a budgeted member while the ledger is degraded, `operator` for an
    /// account the operator disabled, `subscription_ended`): answered as the untimed exhaustion and placed nowhere,
    /// so a probe never moves the session's pin; the name reaches the keepalive, which holds the agent under it.
    Pinned {
        obstacle: &'static str,
    },
}

/// Why the current registry refuses a request admitted under an older one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The registry no longer holds the key the request was admitted on, or holds another key under its id.
    Key(KeyRefusal),
    Resolve(ResolveError),
}

// Boxing the selection would put an allocation on the path every request takes, to shrink a value that is
// built once per selection and moved once into the answering channel.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum SelectResult {
    Exhausted(Exhaustion),
    /// Every account the selection could place on holds its cap; `account_id` is the one it would have chosen.
    Held {
        account_id: String,
        hold: HoldFacts,
    },
    Refused(Refusal),
    Selected(Selection),
}

/// Why a candidate is out of the eligible set. Only a cooldown or a budget carries a deadline the caller can
/// wait for.
enum Obstacle {
    Attempted,
    /// The member's spend reached one of its budgets; `until` is the latest end among the budgets reached and
    /// `period` the budget that end belongs to, the one the record names.
    Budget {
        until: i64,
        period: BudgetPeriod,
    },
    /// The ledger is degraded, so the member's spend rows may be stale; a budgeted member is not placed on until
    /// the writer catches up and the spend is read afresh. Untimed: no deadline clears it.
    BudgetUnknown,
    Cooldown {
        reason: String,
        until: i64,
        window: String,
    },
    /// The poll reads the account's usage credits as unavailable for fast mode.
    Credits {
        reason: String,
    },
    /// Cancelled and past the saved end.
    Ended,
    Operator,
}

/// The step-2 name `release_reasons` and the trace report for an obstacle: `attempted`, `budget`,
/// `budget_unknown`, `cooldown`, `credits`, `operator`, `overage`, or `subscription_ended`.
fn obstacle_name(obstacle: &Obstacle) -> &'static str {
    match obstacle {
        Obstacle::Attempted => "attempted",
        Obstacle::Budget { .. } => "budget",
        Obstacle::BudgetUnknown => "budget_unknown",
        Obstacle::Cooldown { reason, .. } if reason == "overage" => "overage",
        Obstacle::Cooldown { .. } => "cooldown",
        Obstacle::Credits { .. } => "credits",
        Obstacle::Ended => "subscription_ended",
        Obstacle::Operator => "operator",
    }
}

/// The member's spend at or above either of its budgets, still inside the bounds the ledger reported it for:
/// answers the budget reached whose period ends last, with that end. A row of a period the member carries no
/// budget for, or whose bounds do not hold `now`, is no obstacle, and neither is a member with no budget configured.
fn budget_exhausted(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool_id: &str,
    account_id: &str,
    now: i64,
) -> Option<(BudgetPeriod, i64)> {
    let member = budgeted_member(snapshot, pool_id, account_id)?;
    let spends = state
        .member_spends_of(pool_id, account_id)
        .collect::<Vec<_>>();
    budget_reached(member, &spends, now)
}

/// The pool's member for `account_id` while it carries a budget; `None` for a member without one.
fn budgeted_member<'a>(
    snapshot: &'a RegistrySnapshot,
    pool_id: &str,
    account_id: &str,
) -> Option<&'a PoolMember> {
    snapshot
        .members(pool_id)
        .iter()
        .find(|m| m.account_id == account_id)
        .filter(|m| m.budgeted())
}

/// A budgeted member while the ledger is degraded: its spend rows may lack a batch the writer still holds, so what
/// they say about the budget is not trusted either way.
fn budget_unknown(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool_id: &str,
    account_id: &str,
) -> bool {
    state.accounting_degraded() && budgeted_member(snapshot, pool_id, account_id).is_some()
}

/// Whether an account can still serve a request whose allowance is judged in `window` (`window_for`'s answer):
/// for a model with a week of its own, that week and both shared plan windows, since its requests draw on the
/// shared windows too; both shared plan windows otherwise. A prediction at 1.0 is spent. No sample, or a sample
/// that never read the window, counts as allowance (an api_key account, whose sample carries no plan window of
/// its own, always holds it here); a reading past its reset, or one a newer head lowered, no longer excludes.
fn holds_allowance(state: &RoutingState, account_id: &str, window: &str, now: i64) -> bool {
    let spent = |name: &str| {
        state
            .predicted(account_id, name, now)
            .is_some_and(|r| r.utilization >= 1.0)
    };
    if window != SEVEN_DAY {
        !spent(window) && !spent(FIVE_HOUR) && !spent(SEVEN_DAY)
    } else {
        !spent(FIVE_HOUR) && !spent(SEVEN_DAY)
    }
}

/// The latest reset among the spent windows `holds_allowance` reads for `window`, with that window's key: the
/// moment the account can serve the request again once every spent window has reset. `None` when no spent window
/// names a reset.
fn spent_until<'a>(
    state: &RoutingState,
    account_id: &str,
    window: &'a str,
    now: i64,
) -> Option<(i64, &'a str)> {
    [window, FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .filter_map(|name| {
            state
                .predicted(account_id, name, now)
                .filter(|r| r.utilization >= 1.0)
                .and_then(|r| r.reset)
                .map(|reset| (reset, name))
        })
        .max_by_key(|(reset, _)| *reset)
}

/// The pool's affinity in milliseconds, saturating for a value no clock reaches.
pub(crate) fn affinity_ttl_ms(pool: &PoolRow) -> i64 {
    i64::try_from(pool.affinity_ttl_seconds)
        .ok()
        .and_then(|s| s.checked_mul(1_000))
        .unwrap_or(i64::MAX)
}

fn state_name(state: AccountState) -> Option<&'static str> {
    match state {
        AccountState::Ready => None,
        AccountState::Disconnected => Some("disconnected"),
        AccountState::FundingRequired => Some("funding_required"),
        AccountState::ReauthRequired => Some("reauth_required"),
        AccountState::Unpaired => Some("unpaired"),
    }
}

/// The facts a selection finds against a pinned account, in the order it evaluates them, for the event that
/// says why a pin was released without a migration: `expired` (idle past the pool's affinity TTL), `unserved`
/// (the account no longer serves the model for this key: it left the pool, the model was deselected, or the
/// key or the pool is gone), `deleted` (the account left the registry), `paused` (`enabled` is off), the
/// account's state when it cannot serve (`disconnected`, `funding_required`, `reauth_required`, `unpaired`),
/// `subscription_ended` (cancelled and past its saved end), `cooldown` (a live row blocks the model on the
/// account), `fast_rate_limit` or `fast_credits` for a fast request (one of the fast lane's two rows, or the
/// poll's credits state, blocks it), `budget_unknown` (the member carries a budget while the ledger is degraded) or
/// else `budget` (the member's spend is at or above its configured budget), `overage` (in overage under a policy
/// other than `allow`), `attempted` (this request already tried it), and `spent` (no allowance left in the request's
/// window); a fast request draws no plan window, so neither `overage` nor `spent` stands against it. Never empty: a
/// pin released by a move whose destination then answered nothing carries `released`, the one fact the list above
/// does not evaluate.
pub fn release_reasons(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pin: &PinRow,
    request: &SelectRequest,
) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    let pool = snapshot
        .key(&request.key_id)
        .and_then(|k| snapshot.pool(&k.pool_id));
    if pool.is_some_and(|pool| request.now - pin.last_used_at > affinity_ttl_ms(pool)) {
        reasons.push("expired");
    }
    let serves = request
        .candidates
        .iter()
        .any(|c| c.account_id == pin.account_id);
    if pool.is_none() || !serves {
        reasons.push("unserved");
    }
    match snapshot.account(&pin.account_id) {
        None => reasons.push("deleted"),
        Some(account) => {
            if !account.enabled {
                reasons.push("paused");
            }
            if let Some(name) = state_name(account.state) {
                reasons.push(name);
            }
        }
    }
    if snapshot
        .term(&pin.account_id)
        .is_some_and(|term| term::ended(term, request.now))
    {
        reasons.push("subscription_ended");
    }
    if let Some(row) = state.blocking_cooldown(
        &pin.account_id,
        &request.canonical_model,
        request.now,
        request.fast,
    ) {
        // A fast-lane row names its own reason, so the drawer never reads it as a cooled account.
        reasons.push(match row.scope.as_str() {
            "fast" => FAST_RATE_LIMIT_REASON,
            "fast_credits" => FAST_CREDITS_REASON,
            _ => "cooldown",
        });
    }
    // The credits row blocks beside a later row too, and so does the poll's state, which no row records.
    let credits_row = || {
        state
            .cooldowns_of(&pin.account_id, request.now)
            .any(|row| row.scope == "fast_credits")
    };
    let credits_state = || {
        snapshot
            .account(&pin.account_id)
            .and_then(|account| state.credits_of(&pin.account_id, account.incarnation))
            .and_then(CreditsState::blocked)
            .is_some()
    };
    if request.fast && !reasons.contains(&FAST_CREDITS_REASON) && (credits_row() || credits_state())
    {
        reasons.push(FAST_CREDITS_REASON);
    }
    if pool.is_some_and(|p| budget_unknown(state, snapshot, &p.id, &pin.account_id)) {
        reasons.push("budget_unknown");
    } else if pool.is_some_and(|p| {
        budget_exhausted(state, snapshot, &p.id, &pin.account_id, request.now).is_some()
    }) {
        reasons.push("budget");
    }
    if !request.fast
        && pool.is_some_and(|p| p.overage_policy != OveragePolicy::Allow)
        && state.in_overage(&pin.account_id, request.now, request.fallback_ms)
    {
        reasons.push("overage");
    }
    if request.attempted.contains(&pin.account_id) {
        reasons.push("attempted");
    }
    if !request.fast
        && !holds_allowance(
            state,
            &pin.account_id,
            window_for(&request.canonical_model),
            request.now,
        )
    {
        reasons.push("spent");
    }
    if reasons.is_empty() {
        reasons.push("released");
    }
    reasons
}

/// One decision, whole: the answer and its writes as `select` answers them, with the record and the row summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Decided {
    pub result: SelectResult,
    pub writes: Vec<StateWrite>,
    pub record: DecisionRecord,
    pub summary: DecisionSummary,
    /// The round this decision ran, for the task's `SheddingRound` event, or `None` when none ran.
    pub shed_round: Option<ShedRound>,
}

/// A shedding round a decision ran: the pin it ran on and that pin's account, which the round's event names, beside
/// what the round read.
#[derive(Clone, Debug, PartialEq)]
pub struct ShedRound {
    pub account_id: String,
    pub pin: PinKeyFacts,
    pub facts: ShedRoundFacts,
}

/// The selection with its record: every step appends what it read as it read it, so the record and the answer
/// cannot disagree.
pub fn decide(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    rng: &mut impl Rng,
) -> Decided {
    let mut rec = Recorder::default();
    let (result, writes) = run(state, snapshot, request, rng, &mut rec);
    let pool = snapshot
        .key(&request.key_id)
        .and_then(|k| snapshot.pool(&k.pool_id));
    let (record, summary, shed_round) = finish(rec, request, snapshot, pool, &result);
    Decided {
        result,
        writes,
        record,
        summary,
        shed_round,
    }
}

/// The round `move_of` just ran, as the registry keeps it: each pin of the round with its mark and its epoch at the
/// round.
fn round_write(
    state: &RoutingState,
    pool: &PoolRow,
    current: &PinRow,
    at: i64,
    round: &ShedRound,
) -> StateWrite {
    let pins = round
        .facts
        .pins
        .iter()
        .filter_map(|facts| {
            let key = PinKey {
                key_id: facts.key_id.clone(),
                session_id: facts.session_id.clone(),
                agent_id: facts.agent_id.clone(),
                model: facts.model.clone(),
            };
            let epoch = state.pin(&key)?.epoch;
            Some(ShedPin {
                key,
                epoch,
                marked: facts.marked,
            })
        })
        .collect();
    StateWrite::ShedRound {
        pool_id: pool.id.clone(),
        account_id: round.account_id.clone(),
        incarnation: current.incarnation,
        round_at: at,
        pins,
    }
}

/// `decide` without its record: the tests' entry and nothing else's.
pub fn select(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    rng: &mut impl Rng,
) -> (SelectResult, Vec<StateWrite>) {
    let Decided { result, writes, .. } = decide(state, snapshot, request, rng);
    (result, writes)
}

/// The order step 4 runs its rules in, which the walk keeps: a fired rule's successors read `not_run`.
const RULES: [&str; 5] = [
    "move:spent",
    "move:overage",
    "move:return_to_plan",
    "move:drain",
    "move:shed",
];

/// What a decision read while it ran, appended where each fact was read, so the record holds the engine's own
/// readings and never a second look at the state.
#[derive(Default)]
struct Recorder {
    walk: Vec<WalkStep>,
    candidates: Vec<CandidateFacts>,
    pin: Option<PinFacts>,
    pin_reason: Option<&'static str>,
    rule: Option<MigrateReason>,
    source: Option<String>,
    destinations: Vec<String>,
    pin_written: Option<PinWritten>,
    hold_place: Option<&'static str>,
    shed_round: Option<ShedRound>,
    pick: Option<Pick>,
    renewal: Option<Renewal>,
}

/// The pin a renewal kept, as the summary's renewal fact reads it: whether it was cold on this request, the cause
/// `warmth_of` named, and its epoch before the write.
struct Renewal {
    cold: bool,
    cause: Option<&'static str>,
    epoch: u64,
}

/// The pick's account, its tier and every tier member's Pace score (empty under another strategy).
struct Pick {
    account_id: String,
    tier: Tier,
    scores: Vec<Score>,
}

impl Recorder {
    fn step(&mut self, step: &'static str, verdict: &'static str, facts: Vec<Fact>) {
        // A step the table does not list would reach the console as a plain station and fail its fixture test there,
        // so the engine refuses it first, where the author of the new step is.
        debug_assert!(
            WALK_STEPS
                .iter()
                .any(|(id, verdicts)| *id == step && verdicts.contains(&verdict)),
            "the walk step {step}/{verdict} is not in WALK_STEPS"
        );
        self.walk.push(WalkStep {
            step,
            verdict,
            facts,
        });
    }

    /// The member's row, or `None` for an account outside the pool, as an affinity entry another pool's member wrote.
    fn candidate_mut(&mut self, account_id: &str) -> Option<&mut CandidateFacts> {
        self.candidates
            .iter_mut()
            .find(|c| c.account_id == account_id)
    }
}

fn run(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    rng: &mut impl Rng,
    rec: &mut Recorder,
) -> (SelectResult, Vec<StateWrite>) {
    let mut writes = Vec::new();
    let Some(pool) = snapshot
        .key(&request.key_id)
        .and_then(|k| snapshot.pool(&k.pool_id))
    else {
        rec.step("eligible", "none", Vec::new());
        return (SelectResult::Exhausted(Exhaustion::Untimed), writes);
    };
    let ttl_ms = affinity_ttl_ms(pool);
    // A marker-less request writes no cache, so under `when_cached` it has no pin to keep and spreads by the
    // strategy instead of pinning; `always`, a marked request, or a non-portable one still pins.
    let session = request.session_id.as_deref().filter(|_| {
        ttl_ms > 0
            && (pool.affinity.mode == AffinityMode::Always || request.marker || !request.portable)
    });
    if session.is_none() {
        rec.pin_reason = Some("pinning_off");
    }

    // Step 1: the pin, expired when idle past the TTL or when its account can no longer serve the model here. A
    // non-portable request keeps a pin whose account left the pool: the fence below answers for that account,
    // because no other account holds the state the request refers to.
    let mut pin = None;
    if let Some(session) = session {
        let key = PinKey {
            key_id: request.key_id.clone(),
            session_id: session.to_string(),
            agent_id: request.agent_id.clone(),
            model: request.canonical_model.clone(),
        };
        if let Some(current) = state.pin(&key).cloned() {
            let serves = request
                .candidates
                .iter()
                .any(|c| c.account_id == current.account_id);
            let idle = request.now - current.last_used_at > ttl_ms;
            let found = if idle {
                "expired_idle"
            } else if !serves && request.portable {
                "left_pool"
            } else {
                "live"
            };
            let marked = found == "live"
                && state.shed_marked(
                    &pool.id,
                    &key,
                    request.now,
                    seconds_ms(pool.shedding.source_dwell_seconds),
                );
            rec.pin = Some(pin_facts(found, Some(&current), marked, request.now));
            if found == "live" {
                pin = Some(current);
            } else {
                state.remove_pin(&key);
                writes.push(StateWrite::DeletePin(key));
            }
        } else {
            rec.pin = Some(pin_facts("none", None, false, request.now));
        }
    }

    // The fast answer names the pinned account's credits reason first, whatever becomes of the pin below.
    let pinned_account = pin.as_ref().map(|p| p.account_id.clone());

    // What the request says about the pin's cache, judged once where the pin is: every rule below that reads
    // warmth reads this, so a compacted or restarted request is cold on the request that carries the fact.
    let (verdict, cause) = pin.as_ref().map_or((Warmth::Untouched, None), |current| {
        let (verdict, cause) = warmth_of(
            current,
            request,
            &state.affinity().read().expect("affinity lock poisoned"),
        );
        (verdict, Some(cause))
    });
    match rec.pin.as_mut() {
        Some(facts) => {
            if let Some(cause) = cause {
                facts.warmth = Some(verdict.as_str());
                facts.warmth_cause = Some(cause.as_str());
            }
            let line = pin_line(facts);
            rec.step("pin", if pin.is_some() { "match" } else { "none" }, line);
        }
        None => {
            let reason = rec.pin_reason.unwrap_or("pinning_off");
            rec.step("pin", "none", vec![Fact::text("reason", reason)]);
        }
    }

    let window = window_for(&request.canonical_model);

    // Step 2: the eligible set and the obstacle of everything outside it, each kept with its account id for
    // the release event and the trace.
    let mut eligible: Vec<&Candidate> = Vec::new();
    let mut obstacles: Vec<(String, Obstacle)> = Vec::new();
    let mut standing = Vec::with_capacity(request.candidates.len());
    for candidate in &request.candidates {
        let found = obstacle(
            state,
            snapshot,
            pool,
            candidate,
            &request.canonical_model,
            &request.attempted,
            request.now,
            request.fallback_ms,
            request.fast,
        );
        rec.candidates.push(candidate_facts(
            state,
            snapshot,
            pool,
            request,
            candidate,
            window,
            found.as_ref(),
        ));
        standing.push(
            Fact::text("standing", found.as_ref().map_or("eligible", obstacle_name))
                .about(&candidate.account_id),
        );
        match found {
            None => eligible.push(candidate),
            Some(o) => obstacles.push((candidate.account_id.clone(), o)),
        }
    }
    rec.step(
        "eligible",
        if eligible.is_empty() { "none" } else { "match" },
        standing,
    );
    let in_set = |set: &[&Candidate], id: &str| set.iter().any(|c| c.account_id == id);

    // A probe warms the pin it was sent for. With that member at its spend budget, disabled by the operator, past
    // its subscription's end, or already tried by this probe there is nothing to warm, and a placement on a
    // sibling would move the session's pin onto a member no request asked for, so the probe answers the obstacle
    // and moves nothing; the keepalive reads the obstacle's name off the answer.
    if request.probe {
        let mut facts = Vec::new();
        if let Some(current) = pin.as_ref() {
            let pinned = obstacles.iter().find(|(id, _)| id == &current.account_id);
            let tried = request.attempted.iter().any(|a| a == &current.account_id);
            if let Some((_, o)) = pinned {
                facts.push(Fact::text("obstacle", obstacle_name(o)));
            }
            facts.push(Fact::bool("tried", tried));
            let answered = |rec: &mut Recorder, facts: Vec<Fact>, exhaustion: Exhaustion| {
                rec.step("probe", "answered", facts);
                SelectResult::Exhausted(exhaustion)
            };
            match pinned.map(|(_, o)| o) {
                // The window names the obstacle for the relay, which renders it on the wire as the overage claim.
                Some(Obstacle::Budget { until, .. }) => {
                    let exhaustion = Exhaustion::Timed {
                        deadline: *until,
                        window: BUDGET_WINDOW.into(),
                    };
                    return (answered(rec, facts, exhaustion), writes);
                }
                // A probe warms the pin it was sent for; with the member's spend unknown nothing is placed and
                // nothing moves, and no deadline dates the answer.
                Some(o @ Obstacle::BudgetUnknown) => {
                    let exhaustion = Exhaustion::Pinned {
                        obstacle: obstacle_name(o),
                    };
                    return (answered(rec, facts, exhaustion), writes);
                }
                // An attempt of the probe's own hides nothing when no sibling could take it: no pin can move,
                // and the pool's exhaustion (a cooldown's deadline among it) is the truer answer.
                Some(Obstacle::Attempted) if eligible.is_empty() => {}
                // A member this probe tried whose state moved since, as when the attempt's own mark reached the
                // snapshot before this selection, is still the member the probe tried: with a sibling free, a
                // placement there would move the pin all the same.
                Some(Obstacle::Operator)
                    if tried
                        && !eligible.is_empty()
                        && snapshot
                            .account(&current.account_id)
                            .is_some_and(|a| a.enabled) =>
                {
                    let exhaustion = Exhaustion::Pinned {
                        obstacle: obstacle_name(&Obstacle::Attempted),
                    };
                    return (answered(rec, facts, exhaustion), writes);
                }
                // `Operator` also covers a deleted account and a state other than ready; those keep the answer
                // they get under their own reason, and only the operator's pause holds the pin.
                Some(Obstacle::Operator)
                    if snapshot
                        .account(&current.account_id)
                        .is_none_or(|a| a.enabled) => {}
                // No wait clears these, so they answer untimed; the next real request moves the pin if it must.
                Some(o @ (Obstacle::Attempted | Obstacle::Ended | Obstacle::Operator)) => {
                    let exhaustion = Exhaustion::Pinned {
                        obstacle: obstacle_name(o),
                    };
                    return (answered(rec, facts, exhaustion), writes);
                }
                _ => {}
            }
        }
        rec.step("probe", "passed", facts);
    }

    // Step 3: a non-portable request is bound to its pin.
    if let (false, Some(current)) = (request.portable, pin.as_ref()) {
        if in_set(&eligible, &current.account_id) {
            rec.step("bound", "renewed", vec![account_fact(&current.account_id)]);
            return (
                SelectResult::Selected(renew(
                    state,
                    snapshot,
                    pool,
                    current,
                    request,
                    (verdict, cause),
                    &mut writes,
                    rec,
                )),
                writes,
            );
        }
        rec.step(
            "bound",
            "exhausted",
            vec![account_fact(&current.account_id)],
        );
        let exhaustion = fenced(
            state,
            snapshot,
            pool,
            request,
            &current.account_id,
            current.incarnation,
            rec,
        );
        return (SelectResult::Exhausted(exhaustion), writes);
    }

    // Step 4: the moves, for a portable request with a live pin. A pin on an account past its saved end never
    // reaches them: step 5 releases it as it releases a cooled account's. A token count follows its pin: it
    // creates no pin (step 7), so a move it made would leave the session with none.
    let mut migrated = None;
    let mut shed_from = None;
    let mut selection_set: Vec<&Candidate> = eligible.clone();
    if let Some(current) = pin.as_ref() {
        let ended = snapshot
            .term(&current.account_id)
            .is_some_and(|term| term::ended(term, request.now));
        if request.count || ended {
            let reason = if request.count {
                "count"
            } else {
                "subscription_ended"
            };
            for rule in RULES {
                rec.step(rule, "not_run", vec![Fact::text("reason", reason)]);
            }
        } else {
            let cold = !warm_on(current, verdict, request.now);
            // The lane the pin carries once this request renews it: a request that leaves the pin alone stamps
            // none.
            let fast = if verdict == Warmth::Untouched {
                lane_is_fast(&current.lane)
            } else {
                request.fast
            };
            let moved = move_of(
                state, snapshot, pool, request, current, &eligible, window, cold, fast, rec,
            );
            // Ahead of the move's own writes, so the store holds the mark before the pin write that spends it.
            if let Some(round) = rec.shed_round.as_ref() {
                writes.push(round_write(state, pool, current, request.now, round));
            }
            // A hold is for a fresh placement: with every destination at its cap the move is not made, the pin
            // renews in place at step 5, and the next request judges the move again.
            if let Some((reason, set)) = moved {
                let cap = pool.failover.in_flight_cap;
                let loads: Vec<u32> = set
                    .iter()
                    .map(|c| state.in_flight().account(&c.account_id))
                    .collect();
                let open = loads.iter().any(|n| *n < cap);
                rec.step(
                    "cap_filter",
                    if open { "passed" } else { "dropped" },
                    set.iter()
                        .zip(&loads)
                        .map(|(c, n)| in_flight_fact(*n, cap).about(&c.account_id))
                        .collect(),
                );
                if open {
                    state.remove_pin(&current.key());
                    writes.push(StateWrite::DeletePin(current.key()));
                    if reason == MigrateReason::Shed {
                        shed_from = Some(current.account_id.clone());
                    }
                    rec.rule = Some(reason);
                    rec.source = Some(current.account_id.clone());
                    rec.destinations = set.iter().map(|c| c.account_id.clone()).collect();
                    migrated = Some(reason);
                    selection_set = set;
                    pin = None;
                }
            }
        }
    }

    // Step 5: a live pin whose account is eligible is renewed; stickiness wins over priority.
    if let Some(current) = pin.as_ref() {
        if in_set(&eligible, &current.account_id) {
            return (
                SelectResult::Selected(renew(
                    state,
                    snapshot,
                    pool,
                    current,
                    request,
                    (verdict, cause),
                    &mut writes,
                    rec,
                )),
                writes,
            );
        }
    }

    // Step 6: a model with its own window needs allowance left in it and in both shared windows, the cap bounds a
    // fresh placement, the ceiling prefers the accounts under it, the wind-down prefers accounts that can take a
    // fresh session, and under Pace the floor keeps an almost-spent account as the last resort. Each preference
    // falls through to the wider set it narrowed when it would otherwise empty the selection, and every candidate
    // a partition drops joins the trace under that partition's name.
    let mut skipped: Vec<Skipped> = Vec::new();
    // A dated deadline for the accounts the model-window partition passes over, kept apart from `skipped`: it
    // answers `exhaustion_of` when nothing is left to choose from, but every dropped candidate joins the trace as
    // `spent` whether or not its reading names a reset, so the deadline never doubles as its own trace label.
    let mut spent_deadlines: Vec<(String, Obstacle)> = Vec::new();
    // Under `allow`, the spent candidates kept on extra usage: `choose` orders them behind every plan-funded one.
    let mut spent_model: Vec<String> = Vec::new();
    // The candidates placement may choose among, before any of step 6's own partitions: judged on the account
    // itself, never on whether a preference below narrowed the wider set around it.
    let allowed: Vec<&Candidate> = selection_set.clone();
    let mut partitions = Vec::with_capacity(5);
    if window != SEVEN_DAY {
        let mut left: Vec<&Candidate> = Vec::new();
        let mut dropped = 0;
        for c in selection_set.iter().copied() {
            if holds_allowance(state, &c.account_id, window, request.now) {
                left.push(c);
                continue;
            }
            if let Some((until, spent)) = spent_until(state, &c.account_id, window, request.now) {
                spent_deadlines.push((
                    c.account_id.clone(),
                    Obstacle::Cooldown {
                        reason: "spent".into(),
                        until,
                        window: spent.to_string(),
                    },
                ));
            }
            // Extra usage funds every model, so under `allow` a spent week goes on in overage while the poll reads
            // the account's extra usage as enabled; a state the poll has not read is no such reading.
            let funded = pool.overage_policy == OveragePolicy::Allow
                && snapshot
                    .account(&c.account_id)
                    .and_then(|a| state.credits_of(&c.account_id, a.incarnation))
                    .is_some_and(|credits| credits.blocked().is_none());
            if funded {
                spent_model.push(c.account_id.clone());
                left.push(c);
            } else {
                skipped.push(Skipped {
                    account_id: c.account_id.clone(),
                    obstacle: "spent",
                });
                if let Some(member) = rec.candidate_mut(&c.account_id) {
                    member.partition = Some("spent");
                }
                dropped += 1;
            }
        }
        selection_set = left;
        partitions.push(Fact::int("spent", dropped));
        // The tier `choose` orders a member by reads the partition's extra-usage list, which step 2 could not
        // know yet.
        for c in &selection_set {
            let tier = tier_of(
                state,
                snapshot,
                pool,
                c,
                &spent_model,
                request.now,
                request.fallback_ms,
            );
            if let Some(member) = rec.candidate_mut(&c.account_id) {
                member.tier = tier;
            }
        }
    } else {
        partitions.push(Fact::text("spent", "off"));
    }
    // An account at its cap takes no fresh placement: a bound rather than a preference, so it runs ahead of the
    // preferences below and a free account the ceiling would pass over still takes the request. With every
    // account that could serve at its cap the request is held for a slot; the set is left whole so the answer
    // names the account the strategy would have chosen. A pinned request never gets here: step 5 renewed it.
    // A token count is bound to no cap: it occupies no slot (the task charges none for it), so the cap it would
    // be held for is one it never fills. It keeps the whole set and lands where its pin or the strategy says.
    let under_cap: Vec<&Candidate> = selection_set
        .iter()
        .copied()
        .filter(|c| {
            request.count || state.in_flight().account(&c.account_id) < pool.failover.in_flight_cap
        })
        .collect();
    let held = under_cap.is_empty();
    let at_cap = if held && !selection_set.is_empty() {
        Fact::text("at_cap", "held")
    } else {
        narrow(&mut selection_set, &mut skipped, under_cap, "at_cap", rec)
    };
    partitions.push(at_cap);
    // The threshold is a preference, not a refusal: with every account above it the request goes to the least
    // bad one rather than answering exhausted while capacity remains.
    if let Some(threshold) = pool.select_below_utilization {
        let below: Vec<&Candidate> = selection_set
            .iter()
            .copied()
            .filter(|c| {
                state
                    .predicted(&c.account_id, FIVE_HOUR, request.now)
                    .is_none_or(|r| r.utilization < threshold)
            })
            .collect();
        partitions.push(narrow(
            &mut selection_set,
            &mut skipped,
            below,
            "above_ceiling",
            rec,
        ));
    } else {
        partitions.push(Fact::text("above_ceiling", "off"));
    }
    // A session should not start on an account about to end.
    let not_winding_down: Vec<&Candidate> = selection_set
        .iter()
        .copied()
        .filter(|c| {
            snapshot.term(&c.account_id).is_none_or(|term| {
                !term::winding_down(term, request.now, pool.failover.wind_down_minutes)
            })
        })
        .collect();
    partitions.push(narrow(
        &mut selection_set,
        &mut skipped,
        not_winding_down,
        "winding_down",
        rec,
    ));
    // An almost-spent account is kept for when nothing else can serve.
    if pool.strategy == Strategy::Pace {
        let above_floor: Vec<&Candidate> = selection_set
            .iter()
            .copied()
            .filter(|c| {
                remaining(state, &c.account_id, window, request.now).0 > pool.failover.pace_floor
            })
            .collect();
        partitions.push(narrow(
            &mut selection_set,
            &mut skipped,
            above_floor,
            "floor",
            rec,
        ));
    } else {
        partitions.push(Fact::text("floor", "off"));
    }
    rec.step(
        "partitions",
        if selection_set.is_empty() {
            "none"
        } else {
            "match"
        },
        partitions,
    );
    let affinity_handle = state.affinity();
    let affinity_guard = affinity_handle.read().expect("affinity lock poisoned");
    let placed = placement_of(
        state,
        snapshot,
        pool,
        request,
        &affinity_guard,
        &allowed,
        window,
        rec,
    );
    let (chosen, tier, placement) = if let Some((candidate, placement)) = placed {
        (candidate, None, Some(placement))
    } else {
        let Some((strategy_choice, tier)) = choose(
            state,
            snapshot,
            pool,
            window,
            &selection_set,
            &spent_model,
            request.now,
            request.fallback_ms,
            !held,
            rng,
            rec,
        ) else {
            let (pinned, others): (Vec<_>, Vec<_>) = obstacles
                .into_iter()
                .chain(spent_deadlines)
                .partition(|(id, _)| Some(id) == pinned_account.as_ref());
            let weighed: Vec<(String, Obstacle)> = pinned.into_iter().chain(others).collect();
            let names = weighed.iter().map(|(id, o)| weighed_name(id, o)).collect();
            let exhaustion = exhaustion_of(weighed.into_iter().map(|(_, o)| o).collect());
            rec.step(
                "exhausted",
                "exhausted",
                exhausted_facts(&exhaustion, names),
            );
            return (SelectResult::Exhausted(exhaustion), writes);
        };
        if held {
            // The cohort-aware hold is a cap exemption and nothing more: a session already on an account under
            // any agent lands there rather than waiting, and a still-held subagent waits where its own cohort
            // is rather than on the strategy's unrelated choice. A token count never gets here: it is never held.
            let session_pin = any_agent_pin(state, request, &allowed, window, ttl_ms, rec);
            if let Some(candidate) = session_pin {
                rec.step(
                    "hold",
                    "placed",
                    vec![
                        Fact::text("place", "session_pin"),
                        account_fact(&candidate.account_id),
                    ],
                );
                (
                    candidate,
                    None,
                    Some(Placement {
                        rung: "with_main",
                        depth: 0,
                        prefix_tokens: 0,
                    }),
                )
            } else {
                let cohort = subagent_cohort(
                    state,
                    snapshot,
                    pool,
                    request,
                    &affinity_guard,
                    &allowed,
                    window,
                    rec,
                );
                // The wait is the relay's to measure, once it has waited.
                let hold = match cohort {
                    Some(c) => HoldFacts {
                        account_id: c.account_id.clone(),
                        cohort: Some(c.account_id.clone()),
                        held_ms: 0,
                        attempt: request.attempt,
                    },
                    None => HoldFacts {
                        account_id: strategy_choice.account_id.clone(),
                        cohort: None,
                        held_ms: 0,
                        attempt: request.attempt,
                    },
                };
                let place = *rec.hold_place.get_or_insert("strategy_pick");
                rec.step(
                    "hold",
                    "held",
                    vec![
                        Fact::text("place", place),
                        account_fact(&hold.account_id),
                        Fact::int("bound_ms", seconds_ms(pool.failover.retry_in_place_seconds))
                            .unit("ms"),
                    ],
                );
                return (
                    SelectResult::Held {
                        account_id: hold.account_id.clone(),
                        hold,
                    },
                    writes,
                );
            }
        } else {
            for c in &selection_set {
                if c.account_id != strategy_choice.account_id {
                    skipped.push(Skipped {
                        account_id: c.account_id.clone(),
                        obstacle: "ranked",
                    });
                }
            }
            (strategy_choice, Some(tier), None)
        }
    };
    let cohort_hold = cohort_hold(&request.holds);

    // Step 7: a fresh pin with a fresh epoch. A token count touches no cache, so it creates none: a pin it left
    // on an account at its cap would carry the agent's next message past the cap, since a renewal never reads it.
    let pin_ref = session.filter(|_| !request.count).map(|session| {
        let epoch = state.fresh_epoch();
        let row = fresh_pin(request, session, epoch, chosen, shed_from.clone());
        let lane = row.lane.clone();
        rec.pin_written = Some(PinWritten {
            action: if migrated.is_some() {
                "moved"
            } else {
                "created"
            },
            epoch,
            lane: lane.clone(),
            refresh_stamped: true,
            hashes_stamped: request.static_hash.is_some() || !request.boundaries.is_empty(),
        });
        state.put_pin(row.clone());
        writes.push(StateWrite::UpsertPin(row));
        PinRef {
            key_id: request.key_id.clone(),
            session_id: session.to_string(),
            agent_id: request.agent_id.clone(),
            model: request.canonical_model.clone(),
            epoch,
            lane,
        }
    });
    let mut trace_skipped: Vec<Skipped> = obstacles
        .iter()
        .map(|(account_id, o)| Skipped {
            account_id: account_id.clone(),
            obstacle: obstacle_name(o),
        })
        .collect();
    trace_skipped.extend(skipped);
    if placement.is_some() {
        // Rung 3 and `with_main` choose from `allowed`, ahead of step 6's own partitions, so the account they
        // land on can already carry a partition's label here; the placement itself is proof enough that it serves.
        trace_skipped.retain(|s| s.account_id != chosen.account_id);
    }
    (
        SelectResult::Selected(Selection {
            account: selected_row(snapshot, &chosen.account_id),
            upstream_model: chosen.upstream_model.clone(),
            pin: pin_ref,
            migrated,
            in_overage: state.in_overage(&chosen.account_id, request.now, request.fallback_ms),
            trace: SelectionTrace {
                strategy: pool.strategy.as_str(),
                tier,
                renewed: false,
                moved: migrated.map(MigrateReason::as_str),
                skipped: trace_skipped,
                placement,
                cohort_hold,
            },
            pool_id: pool.id.clone(),
        }),
        writes,
    )
}

/// A pin under `session` on `chosen`, created by this request with the request's hashes stamped.
fn fresh_pin(
    request: &SelectRequest,
    session: &str,
    epoch: u64,
    chosen: &Candidate,
    shed_from: Option<String>,
) -> PinRow {
    let mut row = PinRow {
        key_id: request.key_id.clone(),
        session_id: session.to_string(),
        agent_id: request.agent_id.clone(),
        model: request.canonical_model.clone(),
        account_id: chosen.account_id.clone(),
        epoch,
        created_at: request.now,
        last_used_at: request.now,
        // The generation the relay admitted this request under, not the snapshot's: a key deleted and created
        // again under its id between admission and here must not stamp the new holder's generation on a pin
        // this request's old one is choosing.
        key_generation: request.key_generation,
        incarnation: chosen.incarnation,
        burn: PinBurn::default(),
        burn_at: None,
        burn_banded: PinBurn::default(),
        last_input_tokens: 0,
        // A fresh pin has no cache to be cold about, and its TTL is the default until the first report.
        last_refresh_at: request.now,
        cache_ttl_ms: 300_000,
        // The shed pin remembers its source for the dwell, so no round sends it straight back; every other move
        // clears both.
        shed_at: shed_from.is_some().then_some(request.now),
        shed_from,
        static_hash: None,
        tail_hash: None,
        lane: request.lane.clone(),
    };
    stamp_hashes(&mut row, request);
    row
}

/// What a non-portable request bound to `account_id` answers when that account cannot serve it: the account's
/// own obstacle, since no other account holds the state the request refers to.
fn fenced(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    account_id: &str,
    incarnation: u64,
    rec: &mut Recorder,
) -> Exhaustion {
    let own = obstacle(
        state,
        snapshot,
        pool,
        &Candidate {
            account_id: account_id.to_string(),
            incarnation,
            priority: 0,
            upstream_model: String::new(),
        },
        &request.canonical_model,
        &[],
        request.now,
        request.fallback_ms,
        request.fast,
    );
    let names = own.iter().map(|o| weighed_name(account_id, o)).collect();
    let exhaustion = exhaustion_of(own.into_iter().collect());
    rec.step(
        "exhausted",
        "exhausted",
        exhausted_facts(&exhaustion, names),
    );
    exhaustion
}

/// Seconds as milliseconds, saturating at `i64::MAX`, so no product depends on a setting's bound.
pub fn seconds_ms(seconds: u64) -> i64 {
    i64::try_from(seconds)
        .ok()
        .and_then(|s| s.checked_mul(1_000))
        .unwrap_or(i64::MAX)
}

/// The absolute prediction of `window` against `t`, which the select-below gate reads: that ceiling bounds the
/// utilization itself, where the shedding trigger reads its lead over pace.
fn utilization_at_least(
    state: &RoutingState,
    account_id: &str,
    window: &str,
    t: f64,
    now: i64,
) -> bool {
    state
        .predicted(account_id, window, now)
        .is_some_and(|r| r.utilization >= t)
}

const FIVE_HOUR_MS: i64 = 5 * 3_600_000;

/// How far a window's prediction runs ahead of its pace: the utilization less the share of the window that has
/// elapsed, so a reading at 0.5 ten percent into a week is 0.4 ahead and the same reading sixty percent in is
/// behind. A prediction with no reset (a 5-hour window past its reset reads the router's own use since) is ahead
/// by exactly its utilization. Answered as `(lead, elapsed)`, so the record states the pace beside the lead from
/// one reading. `None` without a reading, which every caller treats as under.
fn ahead(state: &RoutingState, account_id: &str, window: &str, now: i64) -> Option<(f64, f64)> {
    let reading = state.predicted(account_id, window, now)?;
    let elapsed = elapsed_share(&reading, window, now);
    Some((reading.utilization - elapsed, elapsed))
}

/// The share of `window` that has elapsed at `now` by the reading's reset, 0 for a reading that names none.
fn elapsed_share(reading: &WindowReading, window: &str, now: i64) -> f64 {
    elapsed_share_at(window, reading.reset, now)
}

/// The share of `window` that has elapsed at `at` by its reset `reset_at`: a week for a `seven_day*` window and
/// five hours otherwise, clamped to the window, and 0 without a reset. The one arithmetic behind the engine's
/// pace reading and the ledger's lead over a raw sample, so the two readings cannot drift apart.
pub fn elapsed_share_at(window: &str, reset_at: Option<i64>, at: i64) -> f64 {
    let length = if window.starts_with(SEVEN_DAY) {
        WEEK_MS
    } else {
        FIVE_HOUR_MS
    };
    reset_at.map_or(0.0, |reset| {
        (length.saturating_sub(reset.saturating_sub(at)) as f64 / length as f64).clamp(0.0, 1.0)
    })
}

/// Whether `account_id` reads cool: a subscription account under `shed_ahead_of_pace` minus the hysteresis band on
/// how far each shared plan window runs ahead of its pace, an api_key account with room in its worst standing bucket
/// (under 1.0, whatever the margin). No threshold configured, or no reading, counts as cool. The reading `move_of`'s
/// `cool` predicate takes for a migration destination and rung 4's `blocked` takes for a placement, so the arithmetic
/// lives once.
fn under_band(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    now: i64,
) -> bool {
    band_facts(state, snapshot, pool, account_id, now).1
}

/// `under_band`'s readings as the record's facts beside its answer: the leads over pace of the two shared windows
/// with their elapsed shares and the larger lead's margin over the band for a subscription account, or the worst
/// bucket for an api_key one, and the line they are held under as `band`: the threshold less `MIGRATE_BAND` on a
/// subscription account, a full bucket (1.0) on an api_key one. No threshold reads as under with no fact.
fn band_facts(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    now: i64,
) -> (Vec<Fact>, bool) {
    let Some(threshold) = pool.shed_ahead_of_pace else {
        return (Vec::new(), true);
    };
    let mut facts = Vec::with_capacity(6);
    let (band, under) = match snapshot.account(account_id).map(|a| a.kind) {
        // A bucket refills within its minute and a reading can be a minute old, so a line under 1.0 says little
        // about the next minute; a destination that hits its limit costs one short wait in place, not a failover.
        Some(AccountKind::ApiKey) => {
            let worst = state.sample(account_id).and_then(|s| s.worst_bucket(now));
            if let Some(reading) = worst {
                facts.push(Fact::number("worst_bucket", reading.utilization).unit("percent"));
            }
            (1.0, worst.is_none_or(|r| r.utilization < 1.0))
        }
        _ => {
            let band = threshold - MIGRATE_BAND;
            let leads = lead_facts(
                &mut facts,
                ahead(state, account_id, FIVE_HOUR, now),
                ahead(state, account_id, SEVEN_DAY, now),
                band,
            );
            (band, leads.iter().flatten().all(|lead| *lead < band))
        }
    };
    facts.push(Fact::number("band", band).unit("percent"));
    (facts, under)
}

/// The two shared windows' leads and elapsed shares as facts, and the larger lead's margin over `against` (the
/// shed threshold or the band): what the shed line and the cool line state of one reading each. Answers the two
/// leads, 5-hour first.
fn lead_facts(
    facts: &mut Vec<Fact>,
    five_hour: Option<(f64, f64)>,
    seven_day: Option<(f64, f64)>,
    against: f64,
) -> [Option<f64>; 2] {
    for (ahead_key, elapsed_key, read) in [
        ("ahead_five_hour", "elapsed_five_hour", five_hour),
        ("ahead_seven_day", "elapsed_seven_day", seven_day),
    ] {
        if let Some((lead, elapsed)) = read {
            facts.push(Fact::number(ahead_key, lead).unit("percent"));
            facts.push(Fact::number(elapsed_key, elapsed).unit("percent"));
        }
    }
    let leads = [
        five_hour.map(|(lead, _)| lead),
        seven_day.map(|(lead, _)| lead),
    ];
    if let Some(larger) = leads.into_iter().flatten().reduce(f64::max) {
        facts.push(Fact::number("margin", larger - against).unit("percent"));
    }
    leads
}

/// Whether the account's allowance is known: an api_key account has no plan window to read, and a subscription
/// account holds a prediction of every window `holds_allowance` reads for the request: the model's own week and
/// both shared windows for a model with a week of its own, both shared windows otherwise.
fn allowance_known(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &str,
    now: i64,
) -> bool {
    let known = |name: &str| state.predicted(account_id, name, now).is_some();
    snapshot.account(account_id).map(|a| a.kind) == Some(AccountKind::ApiKey)
        || if window != SEVEN_DAY {
            known(window) && known(FIVE_HOUR) && known(SEVEN_DAY)
        } else {
            known(FIVE_HOUR) && known(SEVEN_DAY)
        }
}

/// Step 4's rules in their order: spent, overage, return to plan, drain, shed. Each yields the siblings it
/// would move the pin to, and the first with any moves it under its own reason. Each rule appends its line: `no`
/// with the fact that kept it, or `fired` with its destinations and `not_run` for every rule after it.
#[allow(clippy::too_many_arguments)]
fn move_of<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    current: &PinRow,
    eligible: &[&'a Candidate],
    window: &str,
    cold: bool,
    fast: bool,
    rec: &mut Recorder,
) -> Option<(MigrateReason, Vec<&'a Candidate>)> {
    let (now, fallback_ms) = (request.now, request.fallback_ms);
    let prefer_plan = pool.overage_policy == OveragePolicy::PreferPlan;
    let kind = |id: &str| snapshot.account(id).map(|a| a.kind);
    let dwell_ms = seconds_ms(pool.shedding.source_dwell_seconds);
    let cap = pool.failover.in_flight_cap;
    // A sibling the next round would not shed from again soon: every reading its sample holds cool (the shared windows'
    // leads over their pace under the band on a subscription account, room in the worst live bucket of an api_key one;
    // an unsampled account reads as cool), no pool has run a round on it within the dwell, and room under the cap. A
    // full sibling counts as hot, so a move whose cool siblings are all full falls through to a free one rather than
    // being dropped at the cap. The readings come back as the cool line's facts beside the answer.
    let cool_facts = |state: &RoutingState, c: &Candidate| -> (Vec<Fact>, bool) {
        let (mut facts, under) = band_facts(state, snapshot, pool, &c.account_id, now);
        let last_shed = state.newest_shed(&c.account_id);
        let rested = last_shed.is_none_or(|at| now - at >= dwell_ms);
        let in_flight = state.in_flight().account(&c.account_id);
        if let Some(at) = last_shed {
            facts.push(Fact::int("last_shed_age_ms", now - at).unit("ms"));
        }
        facts.push(in_flight_fact(in_flight, cap));
        let cool = under && rested && in_flight < cap;
        facts.push(Fact::bool("cool", cool));
        let facts = facts
            .into_iter()
            .map(|fact| fact.about(&c.account_id))
            .collect();
        (facts, cool)
    };
    let cool = |state: &RoutingState, c: &Candidate| -> bool { cool_facts(state, c).1 };
    let destinations = |state: &RoutingState,
                        keep: &dyn Fn(&Candidate) -> bool|
     -> (Vec<&'a Candidate>, Option<WalkStep>) {
        let set: Vec<&'a Candidate> = eligible
            .iter()
            .copied()
            .filter(|c| c.account_id != current.account_id && keep(c))
            // An account with no allowance left in the model's own window is no destination: step 6 drops it,
            // so releasing the pin for a set of them would strand the request on nothing.
            .filter(|c| window == SEVEN_DAY || holds_allowance(state, &c.account_id, window, now))
            .collect();
        // A hot destination would shed the session again at its next round, so every move prefers a cool
        // one. The preference falls through to the rule's own set: each rule moves the pin for a reason a
        // hot sibling still answers, which staying put does not.
        if pool.shed_ahead_of_pace.is_none() || set.is_empty() {
            return (set, None);
        }
        let mut facts = Vec::new();
        let mut preferred: Vec<&'a Candidate> = Vec::new();
        for c in &set {
            let (read, is_cool) = cool_facts(state, c);
            facts.extend(read);
            if is_cool {
                preferred.push(*c);
            }
        }
        let (set, verdict) = if preferred.is_empty() {
            (set, "fell_through")
        } else {
            (preferred, "preferred")
        };
        (
            set,
            Some(WalkStep {
                step: "cool",
                verdict,
                facts,
            }),
        )
    };

    // A spent window is not a hover: the pinned account cannot serve the request until the window resets, so
    // the pin moves outside the round's budget as soon as a sibling is known to hold allowance, rather than
    // renewing until the upstream's 429 moves it one request later. A sibling holding no reading of the window
    // has an unknown allowance, and a guess could land the session on another spent account, so the upstream
    // decides instead.
    // Under `prefer_plan` a sibling in overage is no destination: the overage rule would hand the session back
    // on the next request, the two rules trading it on no new evidence.
    // A fast request draws usage credits and no plan window, so neither a spent window nor plan overage is a
    // reason to move its session.
    if request.fast {
        rec.step(RULES[0], "no", vec![Fact::bool("fast", true)]);
    } else if !pool.shedding.migrate_when_spent {
        rec.step(
            RULES[0],
            "no",
            vec![Fact::bool("migrate_when_spent", false)],
        );
    } else if holds_allowance(state, &current.account_id, window, now) {
        rec.step(RULES[0], "no", vec![Fact::bool("holds_allowance", true)]);
    } else {
        let (set, cool_line) = destinations(state, &|c| {
            holds_allowance(state, &c.account_id, window, now)
                && allowance_known(state, snapshot, &c.account_id, window, now)
                && !(prefer_plan && state.in_overage(&c.account_id, now, fallback_ms))
        });
        let facts = vec![Fact::bool("holds_allowance", false)];
        if !set.is_empty() {
            fired(rec, 0, facts, &set, cool_line);
            return Some((MigrateReason::Spent, set));
        }
        no_destination(rec, 0, facts);
    }

    // A subscription account in overage hands the session back to plan quota, warm or cold, onto a plan-funded
    // subscription sibling: one out of overage with allowance left, since a spent one would move the pin again
    // on its next request. While the evidence's reset falls within the grace the session keeps its cache: its
    // account is about to get its plan back, and moving now would move it twice.
    let current_kind = kind(&current.account_id);
    if request.fast {
        rec.step(RULES[1], "no", vec![Fact::bool("fast", true)]);
    } else if !prefer_plan {
        rec.step(
            RULES[1],
            "no",
            vec![Fact::text("overage_policy", pool.overage_policy.as_str())],
        );
    } else if current_kind != Some(AccountKind::Oauth) {
        rec.step(RULES[1], "no", vec![kind_fact(current_kind)]);
    } else if !state.in_overage(&current.account_id, now, fallback_ms) {
        rec.step(RULES[1], "no", vec![Fact::bool("in_overage", false)]);
    } else {
        let grace_ms = i64::from(pool.failover.overage_move_grace_minutes) * 60_000;
        let reset = state.overage(&current.account_id).and_then(|e| e.reset);
        let mut facts = vec![Fact::bool("in_overage", true)];
        match reset.filter(|reset| reset - now <= grace_ms) {
            Some(reset) => {
                facts.push(Fact::int("reset_in_grace", reset - now).unit("ms"));
                facts.push(Fact::int("grace_ms", grace_ms).unit("ms"));
                rec.step(RULES[1], "no", facts);
            }
            None => {
                let (set, cool_line) = destinations(state, &|c| {
                    kind(&c.account_id) == Some(AccountKind::Oauth)
                        && !state.in_overage(&c.account_id, now, fallback_ms)
                        && holds_allowance(state, &c.account_id, window, now)
                });
                if !set.is_empty() {
                    fired(rec, 1, facts, &set, cool_line);
                    return Some((MigrateReason::Overage, set));
                }
                no_destination(rec, 1, facts);
            }
        }
    }

    // An api_key member bills every token, so a pin on one returns to plan quota once its cache is cold or once
    // it has sat a dwell on the member. A move writes a fresh row, so `created_at` is the landing here. The
    // member names no reset, so no grace applies.
    let dwell_age_ms = now - current.created_at;
    if current_kind != Some(AccountKind::ApiKey) {
        rec.step(RULES[2], "no", vec![kind_fact(current_kind)]);
    } else {
        let facts = vec![
            Fact::bool("cold", cold),
            Fact::int("dwell_age_ms", dwell_age_ms).unit("ms"),
        ];
        if !(cold || dwell_age_ms >= dwell_ms) {
            rec.step(RULES[2], "no", facts);
        } else {
            let (set, cool_line) = destinations(state, &|c| {
                kind(&c.account_id) == Some(AccountKind::Oauth)
                    && !state.in_overage(&c.account_id, now, fallback_ms)
                    && holds_allowance(state, &c.account_id, window, now)
            });
            if !set.is_empty() {
                fired(rec, 2, facts, &set, cool_line);
                return Some((MigrateReason::ReturnToPlan, set));
            }
            no_destination(rec, 2, facts);
        }
    }

    // A cold pin costs nothing to move off an account about to end. A warm one stays: past the end the
    // `subscription_ended` exclusion releases it and the upstream refuses the dead token.
    let winding_down = |id: &str| {
        snapshot
            .term(id)
            .is_some_and(|term| term::winding_down(term, now, pool.failover.wind_down_minutes))
    };
    if !cold {
        rec.step(RULES[3], "no", vec![Fact::bool("cold", false)]);
    } else if !winding_down(&current.account_id) {
        rec.step(RULES[3], "no", vec![Fact::bool("winding_down", false)]);
    } else {
        let (set, cool_line) = destinations(state, &|c| !winding_down(&c.account_id));
        let facts = vec![Fact::bool("cold", true), Fact::bool("winding_down", true)];
        if !set.is_empty() {
            fired(rec, 3, facts, &set, cool_line);
            return Some((MigrateReason::Drain, set));
        }
        no_destination(rec, 3, facts);
    }

    // The trigger reads how far the prediction runs ahead of the window's pace, as the ceiling reads the
    // prediction itself: a week at 0.5 ten percent in is a session worth shedding, the same week at 0.5 sixty
    // percent in is not, and a refunded window reads zero at its reset.
    let Some(threshold) = pool.shed_ahead_of_pace else {
        rec.step(RULES[4], "no", vec![Fact::text("threshold", "off")]);
        return None;
    };
    let mut facts = Vec::new();
    let leads = [
        (FIVE_HOUR, ahead(state, &current.account_id, FIVE_HOUR, now)),
        (SEVEN_DAY, ahead(state, &current.account_id, SEVEN_DAY, now)),
    ];
    lead_facts(&mut facts, leads[0].1, leads[1].1, threshold);
    facts.push(Fact::number("threshold", threshold).unit("percent"));
    let Some((window, (lead, elapsed))) = leads.into_iter().find_map(|(window, read)| {
        read.filter(|(lead, _)| *lead >= threshold)
            .map(|read| (window, read))
    }) else {
        rec.step(RULES[4], "no", facts);
        return None;
    };
    if state
        .last_shed(&pool.id, &current.account_id)
        .is_none_or(|at| now - at >= dwell_ms)
    {
        let hot = HotWindow {
            window,
            utilization: state
                .predicted(&current.account_id, window, now)
                .map_or(lead + elapsed, |reading| reading.utilization),
            elapsed_share: elapsed,
            ahead: lead,
            threshold,
        };
        let (marks, round) = shed_round(
            state,
            snapshot,
            pool,
            current,
            cold,
            fast,
            now,
            fallback_ms,
            hot,
        );
        facts.push(Fact::text("round", "ran"));
        facts.push(Fact::list(
            "marks",
            marks.iter().map(pin_key_text).collect(),
        ));
        rec.shed_round = Some(ShedRound {
            account_id: current.account_id.clone(),
            pin: pin_key_facts(&current.key()),
            facts: round,
        });
        state.record_shed(&pool.id, &current.account_id, now, marks);
    } else {
        facts.push(Fact::text("round", "within_dwell"));
    }
    // A pin on an account cooled for this model is not renewed at step 5, so the fresh placement replaces it:
    // the release names the cooldown and the mark leaves with the pin, where a shed would name a move the
    // cooldown forced. The round above still runs, since a cooldown can hold one model alone.
    if state
        .blocking_cooldown(
            &current.account_id,
            &request.canonical_model,
            now,
            request.fast,
        )
        .is_some()
    {
        facts.push(Fact::bool("cooldown", true));
        rec.step(RULES[4], "no", facts);
        return None;
    }
    // A pin moves only when its session asks, so an unmarked pin never moves for utilization and a marked one
    // moves at its next request. A plan-quota round sheds window share, which a fast request does not draw, so
    // its mark moves a standard request alone; a dollar round's mark moves a fast one too.
    let marked = state.shed_marked(&pool.id, &current.key(), now, dwell_ms);
    facts.push(Fact::bool("marked", marked));
    if !marked {
        rec.step(RULES[4], "no", facts);
        return None;
    }
    if request.fast
        && burn_unit(state, snapshot, &current.account_id, now, fallback_ms)
            == BurnUnit::WindowShare
    {
        facts.push(Fact::bool("window_share_fast", true));
        rec.step(RULES[4], "no", facts);
        return None;
    }
    let (set, cool_line) = destinations(state, &|c| {
        let shed_it_here = current.shed_from.as_deref() == Some(c.account_id.as_str())
            && current.shed_at.is_some_and(|at| now - at < dwell_ms);
        cool(state, c) && !shed_it_here
    });
    if set.is_empty() {
        no_destination(rec, 4, facts);
        return None;
    }
    fired(rec, 4, facts, &set, cool_line);
    Some((MigrateReason::Shed, set))
}

/// A rule that moves the pin: its line with the destinations, the rules after it `not_run`, then the cool reading
/// the destinations were preferred by.
fn fired(
    rec: &mut Recorder,
    rule: usize,
    mut facts: Vec<Fact>,
    set: &[&Candidate],
    cool: Option<WalkStep>,
) {
    facts.push(Fact::list(
        "destinations",
        set.iter().map(|c| c.account_id.clone()).collect(),
    ));
    rec.step(RULES[rule], "fired", facts);
    for later in &RULES[rule + 1..] {
        rec.step(later, "not_run", Vec::new());
    }
    rec.walk.extend(cool);
}

/// A rule whose reason held but which found no sibling to move the pin to.
fn no_destination(rec: &mut Recorder, rule: usize, mut facts: Vec<Fact>) {
    facts.push(Fact::bool("no_destination", true));
    rec.step(RULES[rule], "no", facts);
}

/// The unit the account's burn is shed in: the plan window's share on a subscription account on plan quota,
/// dollars on an api_key account or one in overage.
fn burn_unit(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    now: i64,
    fallback_ms: i64,
) -> BurnUnit {
    let kind = snapshot
        .account(account_id)
        .map_or(AccountKind::Oauth, |a| a.kind);
    BurnUnit::of(kind, state.in_overage(account_id, now, fallback_ms))
}

/// The window whose lead over pace fired a shedding round, as `ahead` read it, with the threshold it reached.
struct HotWindow {
    window: &'static str,
    utilization: f64,
    elapsed_share: f64,
    ahead: f64,
    threshold: f64,
}

/// A shedding round over the pool's pins on the account `current` sits on: every pin of it under one of the pool's
/// keys, cold first by burn, then warm by burn over rewrite cost, bounded by `warm_sessions`, marked in that order
/// up to `shed_share` of that slice's burn in the unit its funding selects. On plan quota each burn is the
/// subscription cost model's 5-hour share rate on its anchor tier: the tier's capacity ratio divides every pin of
/// the account alike, so neither the order nor the share of the sum needs it, and a pin on a fast lane is out of the
/// round, since it draws no window and has nothing to shed there; in dollars it stays. The current pin's warmth and
/// lane are this request's. Every other pin's are the stored ones. Answers the marks with what the round read, `hot`
/// the window that fired it.
#[allow(clippy::too_many_arguments)]
fn shed_round(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    current: &PinRow,
    current_cold: bool,
    current_fast: bool,
    now: i64,
    fallback_ms: i64,
    hot: HotWindow,
) -> (Vec<PinKey>, ShedRoundFacts) {
    let account_id = &current.account_id;
    let unit = burn_unit(state, snapshot, account_id, now, fallback_ms);
    // The round is the account's one pool's: a pin under a key of a pool the account left is retained until it ends
    // and is neither ranked nor marked here, so a former pool's pins never spend this pool's mark budget.
    let in_pool = |p: &PinRow| {
        snapshot
            .key(&p.key_id)
            .is_some_and(|k| k.pool_id == pool.id)
    };
    let (mut pins, mut sum) = state.burn_of(
        account_id,
        &in_pool,
        unit,
        state.rates(),
        now,
        seconds_ms(pool.shedding.burn_window_seconds),
    );
    let is_current = |p: &PinRow| p.key() == current.key();
    let is_cold = |p: &PinRow| {
        if is_current(p) {
            current_cold
        } else {
            !warm(p, now)
        }
    };
    let mut left_out = Vec::new();
    if unit == BurnUnit::WindowShare {
        let on_fast_lane = |p: &PinRow| {
            if is_current(p) {
                current_fast
            } else {
                lane_is_fast(&p.lane)
            }
        };
        (left_out, pins) = pins.into_iter().partition(|(p, _)| on_fast_lane(p));
        sum = pins.iter().filter_map(|(_, burn)| *burn).sum();
    }
    let (mut cold, mut warm_pins): (Vec<_>, Vec<_>) =
        pins.into_iter().partition(|(p, _)| is_cold(p));
    // A pin whose burn cannot be priced ranks after every priced pin of its group: a guess never orders a shed.
    let priced_first =
        |a: &Option<f64>, b: &Option<f64>, desc: fn(f64, f64) -> Ordering| match (a, b) {
            (Some(a), Some(b)) => desc(*a, *b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
    // A pin with no prompt size reported yet ranks as the cheapest rewrite.
    let per_rewrite =
        |(p, burn): &(PinRow, Option<f64>)| burn.map(|b| b / p.last_input_tokens.max(1) as f64);
    let by_rewrite = |a: &(PinRow, Option<f64>), b: &(PinRow, Option<f64>)| {
        priced_first(&per_rewrite(a), &per_rewrite(b), |x, y| y.total_cmp(&x))
    };
    cold.sort_by(|a, b| priced_first(&a.1, &b.1, |x, y| y.total_cmp(&x)));
    warm_pins.sort_by(by_rewrite);
    let cold_priced = cold.iter().filter(|(_, burn)| burn.is_some()).count();
    let mut ordered = cold;
    ordered.extend(warm_pins);
    if pool.shedding.warm_sessions == WarmSessions::Always {
        ordered.sort_by(by_rewrite);
    }
    let target = pool.shedding.shed_share * sum;
    let mut marks = shed_marks(&ordered, target);
    // `never` stops after the cold pins. The warm ones stay in the order all the same, so the last-pin rule
    // still counts them among the account's pins; the marks are priced pins alone, so the cold group's marks are
    // its priced count at most.
    if pool.shedding.warm_sessions == WarmSessions::Never {
        marks.truncate(cold_priced);
    }
    let pin_facts = |pin: &(PinRow, Option<f64>), left: bool| {
        let (p, burn) = pin;
        ShedPinFacts {
            key_id: p.key_id.clone(),
            session_id: p.session_id.clone(),
            agent_id: p.agent_id.clone(),
            model: p.model.clone(),
            cold: is_cold(p),
            burn: *burn,
            last_input_tokens: p.last_input_tokens,
            per_rewrite: per_rewrite(pin),
            marked: marks.contains(&p.key()),
            note: if left {
                Some("fast_lane")
            } else if burn.is_none() {
                Some("unpriced")
            } else if is_current(p) {
                Some("current")
            } else {
                None
            },
        }
    };
    let dwell_seconds = pool.shedding.source_dwell_seconds;
    let round = ShedRoundFacts {
        window: hot.window.to_string(),
        utilization: hot.utilization,
        elapsed_share: hot.elapsed_share,
        ahead: hot.ahead,
        threshold: hot.threshold,
        margin: hot.ahead - hot.threshold,
        unit: unit.as_str(),
        burn_window_seconds: pool.shedding.burn_window_seconds,
        shed_share: pool.shedding.shed_share,
        burn_sum: sum,
        target,
        warm_sessions: pool.shedding.warm_sessions.as_str(),
        pins: ordered
            .iter()
            .map(|pin| pin_facts(pin, false))
            .chain(left_out.iter().map(|pin| pin_facts(pin, true)))
            .collect(),
        marks: marks.iter().map(pin_key_facts).collect(),
        dwell_seconds,
        dwell_until: now.saturating_add(seconds_ms(dwell_seconds)),
    };
    (marks, round)
}

fn pin_key_facts(key: &PinKey) -> PinKeyFacts {
    PinKeyFacts {
        key_id: key.key_id.clone(),
        session_id: key.session_id.clone(),
        agent_id: key.agent_id.clone(),
        model: key.model.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn obstacle(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    candidate: &Candidate,
    model: &str,
    attempted: &[String],
    now: i64,
    fallback_ms: i64,
    fast: bool,
) -> Option<Obstacle> {
    let tried = attempted.iter().any(|a| a == &candidate.account_id);
    // A candidate the registry no longer holds was deleted after admission; it is nobody's to select, and a
    // cooldown row it left behind must not date an exhaustion no wait can clear.
    let Some(account) = snapshot.account(&candidate.account_id) else {
        return Some(Obstacle::Operator);
    };
    if !account.enabled {
        return Some(Obstacle::Operator);
    }
    // A funding mark is tried again on its own clock, one request at a time, since an API-key balance has no signal
    // the poll could read.
    if account.state == AccountState::FundingRequired {
        if !state.funding_retry_admits(&candidate.account_id, now) {
            return Some(Obstacle::Operator);
        }
    } else if account.state != AccountState::Ready {
        return Some(Obstacle::Operator);
    }
    if snapshot
        .term(&candidate.account_id)
        .is_some_and(|term| term::ended(term, now))
    {
        return Some(Obstacle::Ended);
    }
    let cooldown = |row: &CooldownRow| Obstacle::Cooldown {
        reason: row.reason.clone(),
        until: row.until,
        window: row.window.clone(),
    };
    let blocking = state.blocking_cooldown(&candidate.account_id, model, now, fast);
    // A fast rate limit dates the fast lane alone: where the account's credits (their row or the poll's state) or
    // its budget block the request too, that blocker answers, since the fast reset would pass with the request
    // still refused.
    let fast_limited = blocking.filter(|row| row.scope == "fast");
    if let Some(row) = blocking.filter(|_| fast_limited.is_none()) {
        return Some(cooldown(row));
    }
    if fast {
        if let Some(row) = state
            .cooldowns_of(&candidate.account_id, now)
            .find(|row| row.scope == "fast_credits")
        {
            return Some(cooldown(row));
        }
        if let Some(reason) = state
            .credits_of(&candidate.account_id, account.incarnation)
            .and_then(CreditsState::blocked)
        {
            return Some(Obstacle::Credits {
                reason: reason.into(),
            });
        }
    }
    if budget_unknown(state, snapshot, &pool.id, &candidate.account_id) {
        return Some(Obstacle::BudgetUnknown);
    }
    if let Some((period, until)) =
        budget_exhausted(state, snapshot, &pool.id, &candidate.account_id, now)
    {
        // A period that ends inside the fast window would answer a retry the fast lane still refuses.
        return Some(Obstacle::Budget {
            until: fast_limited.map_or(until, |row| until.max(row.until)),
            period,
        });
    }
    if let Some(row) = fast_limited {
        return Some(cooldown(row));
    }
    // Plan overage is a plan window's state, which a fast request does not draw.
    if !fast && pool.overage_policy == OveragePolicy::Never {
        if let Some(deadline) = state
            .overage_deadline(&candidate.account_id, fallback_ms)
            .filter(|d| *d > now)
        {
            return Some(Obstacle::Cooldown {
                reason: "overage".into(),
                until: deadline,
                window: "overage".into(),
            });
        }
    }
    // An account this request already tried is out of the set either way, so its cooldown is reported ahead of
    // the attempt: three accounts rate-limited inside one request answer with the earliest reset, not untimed.
    if tried {
        return Some(Obstacle::Attempted);
    }
    None
}

/// Renews the pin and persists the request's verdict on it: a refreshing request stamps its refresh and its
/// hashes, a cold one zeroes the refresh and stamps its hashes so the next continuation re-warms the pin, and
/// a foreign one leaves both alone. A request that carries no chain has no hashes to stamp, so the pin keeps
/// the last conversation it was judged against; its lane is stamped all the same, or a chainless body on a new
/// lane would leave the pin cold for good.
#[allow(clippy::too_many_arguments)]
fn renew(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    current: &PinRow,
    request: &SelectRequest,
    (verdict, cause): (Warmth, Option<WarmthCause>),
    writes: &mut Vec<StateWrite>,
    rec: &mut Recorder,
) -> Selection {
    let (candidates, now, fallback_ms) = (&request.candidates, request.now, request.fallback_ms);
    let mut renewed = PinRow {
        last_used_at: now,
        ..current.clone()
    };
    let chained = request.static_hash.is_some() || !request.boundaries.is_empty();
    match verdict {
        Warmth::Refresh => renewed.last_refresh_at = now,
        Warmth::Cold => renewed.last_refresh_at = 0,
        Warmth::Untouched => {}
    }
    if verdict != Warmth::Untouched {
        renewed.lane = request.lane.clone();
        if chained {
            stamp_hashes(&mut renewed, request);
        }
    }
    let lane = renewed.lane.clone();
    let (refresh_stamped, hashes_stamped) = (
        verdict == Warmth::Refresh,
        verdict != Warmth::Untouched && chained,
    );
    rec.renewal = Some(Renewal {
        cold: !warm_on(current, verdict, now),
        cause: cause.map(WarmthCause::as_str),
        epoch: current.epoch,
    });
    rec.step(
        "renew",
        "renewed",
        vec![
            account_fact(&current.account_id),
            Fact::text("lane", lane.as_str()),
            Fact::bool("refresh_stamped", refresh_stamped),
            Fact::bool("hashes_stamped", hashes_stamped),
        ],
    );
    rec.pin_written = Some(PinWritten {
        action: "renewed",
        epoch: current.epoch,
        lane: lane.clone(),
        refresh_stamped,
        hashes_stamped,
    });
    state.put_pin(renewed.clone());
    state.mark_renewed(&current.key());
    writes.push(StateWrite::UpsertPin(renewed));
    Selection {
        account: selected_row(snapshot, &current.account_id),
        upstream_model: candidates
            .iter()
            .find(|c| c.account_id == current.account_id)
            .map(|c| c.upstream_model.clone())
            .expect(
                "a renewed pin passed the eligible-set test, which is built from the candidates",
            ),
        pin: Some(PinRef {
            key_id: current.key_id.clone(),
            session_id: current.session_id.clone(),
            agent_id: current.agent_id.clone(),
            model: current.model.clone(),
            epoch: current.epoch,
            lane,
        }),
        migrated: None,
        in_overage: state.in_overage(&current.account_id, now, fallback_ms),
        trace: SelectionTrace {
            strategy: pool.strategy.as_str(),
            renewed: true,
            ..SelectionTrace::default()
        },
        pool_id: pool.id.clone(),
    }
}

/// Every path that selects an account has already passed it through `obstacle`, which answers `Operator` for
/// an id the snapshot does not hold, so the row is there.
fn selected_row(snapshot: &RegistrySnapshot, account_id: &str) -> AccountRow {
    snapshot
        .account(account_id)
        .cloned()
        .expect("a selected account is one the snapshot the selection ran against holds")
}

/// Replaces `selection_set` with `kept` when `kept` is not empty (a preference that falls through rather than
/// stranding the request when it would empty the set), recording every dropped candidate under `label` in the
/// trace and on its candidate row. Answers the partition's fact: the count dropped, or `fell_through`.
fn narrow<'a>(
    selection_set: &mut Vec<&'a Candidate>,
    skipped: &mut Vec<Skipped>,
    kept: Vec<&'a Candidate>,
    label: &'static str,
    rec: &mut Recorder,
) -> Fact {
    if kept.is_empty() {
        return if selection_set.is_empty() {
            Fact::int(label, 0)
        } else {
            Fact::text(label, "fell_through")
        };
    }
    let mut dropped = 0;
    for c in selection_set.iter() {
        if !kept.iter().any(|k| k.account_id == c.account_id) {
            skipped.push(Skipped {
                account_id: c.account_id.clone(),
                obstacle: label,
            });
            if let Some(member) = rec.candidate_mut(&c.account_id) {
                member.partition = Some(label);
            }
            dropped += 1;
        }
    }
    *selection_set = kept;
    Fact::int(label, dropped)
}

/// Whether `account_id` may take a fresh placement: it is among the candidates placement chooses among
/// (`allowed`, the selection set before step 6's own partitions) and still holds allowance in the request's
/// window.
fn admits(
    state: &RoutingState,
    allowed: &[&Candidate],
    window: &str,
    account_id: &str,
    now: i64,
) -> bool {
    allowed.iter().any(|c| c.account_id == account_id)
        && holds_allowance(state, account_id, window, now)
}

fn account_ok(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    allowed: &[&Candidate],
    window: &str,
    now: i64,
    entry: &AffinityEntry,
) -> bool {
    account_refusal(state, snapshot, allowed, window, now, entry).is_none()
}

/// What keeps placement off an affinity entry's account, `None` for one it may land on: `expired` (not live),
/// `incarnation` (another holder of the id wrote it), `not_admitted` (the account admits no fresh placement).
/// Shared by every rung. Rung 4 adds its own blocker on top.
fn account_refusal(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    allowed: &[&Candidate],
    window: &str,
    now: i64,
    entry: &AffinityEntry,
) -> Option<&'static str> {
    if !entry.live(now) {
        Some("expired")
    } else if snapshot.account(&entry.account_id).map(|a| a.incarnation) != Some(entry.incarnation)
    {
        Some("incarnation")
    } else if !admits(state, allowed, window, &entry.account_id, now) {
        Some("not_admitted")
    } else {
        None
    }
}

/// Rung 4's own gate on a candidate account, evaluated per candidate rather than inferred from the
/// post-partition selection set: under the cap, below `select_below_utilization` on the 5-hour window when the
/// pool sets one, not winding down, above the Pace floor under Pace, and under `under_band`, the reading
/// `move_of`'s `cool` takes for a migration destination. Answers the first gate that blocks it: `at_cap`,
/// `above_ceiling`, `winding_down`, `floor` or `band`.
fn blocked(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    now: i64,
    account_id: &str,
) -> Option<&'static str> {
    if state.in_flight().account(account_id) >= pool.failover.in_flight_cap {
        Some("at_cap")
    } else if pool
        .select_below_utilization
        .is_some_and(|threshold| utilization_at_least(state, account_id, FIVE_HOUR, threshold, now))
    {
        Some("above_ceiling")
    } else if snapshot
        .term(account_id)
        .is_some_and(|term| term::winding_down(term, now, pool.failover.wind_down_minutes))
    {
        Some("winding_down")
    } else if pool.strategy == Strategy::Pace
        && remaining(state, account_id, window, now).0 <= pool.failover.pace_floor
    {
        Some("floor")
    } else if !under_band(state, snapshot, pool, account_id, now) {
        Some("band")
    } else {
        None
    }
}

/// A fresh request with no live pin lands where its prefix is already warm: the deepest message boundary
/// (rung 3, always, bypassing the cap as a pin renewal does, since a tail match is the conversation's own
/// account); else, behind the class knob, the parent agent's pin (`with_main`, bypassing the cap the same way,
/// since the cohort's own account outweighs it) or the deepest static boundary past the size gate, the cache
/// gate and the blocker (rung 4, already under the cap by construction). `allowed` is the selection set before
/// step 6's own partitions, so the source of a move or an account a preference narrowed around can never win a
/// placement it would not have won on its own account. A token count is never placed: it reads no cache, so a
/// warm prefix would only lift it past the cap and promise its trace a read it can never make.
#[allow(clippy::too_many_arguments)]
fn placement_of<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    affinity: &AffinityMap,
    allowed: &[&'a Candidate],
    window: &str,
    rec: &mut Recorder,
) -> Option<(&'a Candidate, Placement)> {
    let none = |rec: &mut Recorder, rung: &'static str, reason: &'static str| {
        rec.step(rung, "none", vec![Fact::text("reason", reason)]);
    };
    if request.count {
        for rung in ["rung:content", "rung:with_main", "rung:static"] {
            none(rec, rung, "count");
        }
        return None;
    }
    let now = request.now;
    let candidate_of =
        |account_id: &str| allowed.iter().copied().find(|c| c.account_id == account_id);
    let entry_gate = |e: &AffinityEntry| {
        if same_lane(&e.lane, &request.lane) {
            account_refusal(state, snapshot, allowed, window, now, e)
        } else {
            Some("lane")
        }
    };

    let content = affinity.walk(&request.canonical_model, &request.boundaries, entry_gate);
    record_lookups(rec, &content, now, true);
    let matched = content.matched.map(|i| &content.visited[i]);
    if let Some((visited, candidate)) =
        matched.and_then(|v| candidate_of(&v.entry.account_id).map(|c| (v, c)))
    {
        rec.step("rung:content", "match", rung_facts(visited));
        rec.step("rung:with_main", "not_run", Vec::new());
        rec.step("rung:static", "not_run", Vec::new());
        return Some((
            candidate,
            Placement {
                rung: "content",
                depth: visited.depth,
                prefix_tokens: visited.entry.prefix_tokens,
            },
        ));
    }
    none(
        rec,
        "rung:content",
        if matched.is_some() {
            "not_admitted"
        } else {
            "no_entry"
        },
    );

    let is_subagent = request.agent_id != "main";
    if !is_subagent {
        none(rec, "rung:with_main", "class");
    } else if pool.affinity.subagent_placement != SubagentPlacement::WithMain {
        none(rec, "rung:with_main", "policy_off");
    } else {
        // The class knob places a subagent with its parent or not at all: the static rung never runs under it.
        let parent = request.session_id.as_deref().and_then(|session_id| {
            state.pin(&PinKey {
                key_id: request.key_id.clone(),
                session_id: session_id.to_string(),
                agent_id: request
                    .parent_agent_id
                    .clone()
                    .unwrap_or_else(|| "main".to_string()),
                model: request.canonical_model.clone(),
            })
        });
        let Some(pin) = parent else {
            none(rec, "rung:with_main", "no_pin");
            rec.step("rung:static", "not_run", Vec::new());
            return None;
        };
        if let Some(member) = rec.candidate_mut(&pin.account_id) {
            member.prefix.with_main = true;
        }
        let refusal = if now - pin.last_used_at > affinity_ttl_ms(pool) {
            Some("pin_idle")
        } else if !admits(state, allowed, window, &pin.account_id, now) {
            Some("not_admitted")
        } else {
            None
        };
        let placed = match refusal {
            None => candidate_of(&pin.account_id),
            Some(_) => None,
        };
        let Some(candidate) = placed else {
            rec.step(
                "rung:with_main",
                "none",
                vec![
                    Fact::text("reason", refusal.unwrap_or("not_admitted")),
                    account_fact(&pin.account_id),
                ],
            );
            rec.step("rung:static", "not_run", Vec::new());
            return None;
        };
        rec.step(
            "rung:with_main",
            "match",
            vec![account_fact(&candidate.account_id)],
        );
        rec.step("rung:static", "not_run", Vec::new());
        return Some((
            candidate,
            Placement {
                rung: "with_main",
                depth: 0,
                prefix_tokens: 0,
            },
        ));
    }
    let by_warm_prefix = if is_subagent {
        pool.affinity.subagent_placement == SubagentPlacement::ByCohort
    } else {
        pool.affinity.session_placement == SessionPlacement::ByWarmPrefix
    };
    if !by_warm_prefix {
        none(rec, "rung:static", "policy_off");
        return None;
    }
    if !request.marker && request.portable {
        none(rec, "rung:static", "cache_gate");
        return None;
    }
    // An entry whose estimate is zero (the empty-tools boundary of a request without tools) never places for
    // any class; the threshold itself binds a main conversation only, since a subagent cohort lands together on
    // any shared prefix.
    let size_gate = |entry: &AffinityEntry| {
        entry.prefix_tokens > 0
            && (is_subagent || entry.prefix_tokens >= pool.affinity.min_shared_prefix_tokens)
    };
    let walk = affinity.walk(&request.canonical_model, &request.static_boundaries, |e| {
        entry_gate(e)
            .or_else(|| (!size_gate(e)).then_some("size_gate"))
            .or_else(|| blocked(state, snapshot, pool, window, now, &e.account_id))
    });
    record_lookups(rec, &walk, now, false);
    let Some(visited) = walk.matched.map(|i| &walk.visited[i]) else {
        none(rec, "rung:static", "no_entry");
        return None;
    };
    let Some(candidate) = candidate_of(&visited.entry.account_id) else {
        none(rec, "rung:static", "not_admitted");
        return None;
    };
    rec.step("rung:static", "match", rung_facts(visited));
    Some((
        candidate,
        Placement {
            rung: "static",
            depth: visited.depth,
            prefix_tokens: visited.entry.prefix_tokens,
        },
    ))
}

fn rung_facts(visited: &Visited<'_>) -> Vec<Fact> {
    vec![
        Fact::count("depth", u64::from(visited.depth)).unit("count"),
        Fact::count("prefix_tokens", visited.entry.prefix_tokens).unit("tokens"),
        account_fact(&visited.entry.account_id),
        Fact::text("hash", visited.hash),
    ]
}

/// Each visited member's lookup for the rung: its deepest visited entry, with the gate that refused it, and apart
/// from it the deepest one the gate accepted, so a deeper refusal never stands for a shallower placement.
fn record_lookups(rec: &mut Recorder, walk: &RungWalk<'_>, now: i64, content: bool) {
    for visited in &walk.visited {
        let Some(member) = rec.candidate_mut(&visited.entry.account_id) else {
            continue;
        };
        let lookup = &mut member.prefix;
        let (deepest, matched) = if content {
            (&mut lookup.content, &mut lookup.content_match)
        } else {
            (&mut lookup.static_, &mut lookup.static_match)
        };
        let accepted = visited.refused_by.is_none() && matched.is_none();
        if deepest.is_some() && !accepted {
            continue;
        }
        let entry = PrefixEntry {
            hash: visited.hash.to_string(),
            depth: visited.depth,
            prefix_tokens: visited.entry.prefix_tokens,
            written_at: visited.entry.written_at,
            ttl_ms: visited.entry.ttl_ms,
            live: visited.entry.live(now),
            refused_by: visited.refused_by,
        };
        if accepted {
            *matched = Some(entry.clone());
        }
        if deepest.is_none() {
            *deepest = Some(entry);
        }
    }
}

/// The cohort-aware hold's first override: a session already on an account under any agent is never truly
/// held, whichever agent's turn this is. A stale pin of the request's own agent can sort before an admissible
/// one of another, so every pin of the session is checked rather than just the first the sort finds.
fn any_agent_pin<'a>(
    state: &RoutingState,
    request: &SelectRequest,
    allowed: &[&'a Candidate],
    window: &str,
    idle_ms: i64,
    rec: &mut Recorder,
) -> Option<&'a Candidate> {
    let session_id = request.session_id.as_deref()?;
    let account_id = state
        .session_pins(&request.key_id, session_id, &request.canonical_model)
        .find(|p| {
            request.now - p.last_used_at <= idle_ms
                && admits(state, allowed, window, &p.account_id, request.now)
        })?
        .account_id
        .clone();
    let found = allowed.iter().copied().find(|c| c.account_id == account_id);
    if found.is_some() {
        rec.hold_place = Some("session_pin");
    }
    found
}

/// The cohort-aware hold's second override: a subagent still held after the first waits where its own cohort
/// is, so the wait frees a slot there rather than on the strategy's unrelated choice. Gated as rung 4 is (the
/// zero-size entry, the class knob, the cache gate), so an empty-tools boundary or a strategy-only pool never
/// names a false cohort.
#[allow(clippy::too_many_arguments)]
fn subagent_cohort<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    affinity: &AffinityMap,
    allowed: &[&'a Candidate],
    window: &str,
    rec: &mut Recorder,
) -> Option<&'a Candidate> {
    if request.agent_id == "main"
        || pool.affinity.subagent_placement != SubagentPlacement::ByCohort
        || (!request.marker && request.portable)
    {
        return None;
    }
    let (_, entry) =
        affinity.deepest(&request.canonical_model, &request.static_boundaries, |e| {
            same_lane(&e.lane, &request.lane)
                && e.prefix_tokens > 0
                && account_ok(state, snapshot, allowed, window, request.now, e)
        })?;
    let found = allowed
        .iter()
        .copied()
        .find(|c| c.account_id == entry.account_id);
    if found.is_some() {
        rec.hold_place = Some("cohort");
    }
    found
}

const WEEK_HOURS: f64 = 168.0;

/// Pace's remaining allowance for a candidate at `now`, and the predicted reading's reset when it came from the
/// candidate's own `window` (an api_key account's worst-bucket reset never enters the score). A subscription
/// window nobody has read, an api_key account with every bucket refunded or aged, or an account never sampled,
/// all read as untouched: remaining 1, no reset. A reading past its reset reads as the router's own use since it.
fn remaining(state: &RoutingState, account_id: &str, window: &str, now: i64) -> (f64, Option<i64>) {
    if let Some(reading) = state.predicted(account_id, window, now) {
        return ((1.0 - reading.utilization).max(0.0), reading.reset);
    }
    match state.sample(account_id).and_then(|s| s.worst_bucket(now)) {
        Some(reading) => ((1.0 - reading.utilization).max(0.0), None),
        None => (1.0, None),
    }
}

/// Pace's inputs and score for a candidate at `now`: remaining allowance over the hours to the earlier of the
/// window's reset and the account's derived end, so an account that runs out sooner is spent first. An api_key
/// account scores by its worst live bucket alone (a bucket refills within a minute, so Pace degrades to least
/// utilized on it, and the bucket's own reset never enters the score). `choose` ranks by this score and the
/// record carries the rest, so the two cannot disagree.
fn pace_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    candidate: &Candidate,
    window: &str,
    now: i64,
) -> PaceFacts {
    let (remaining, reset) = remaining(state, &candidate.account_id, window, now);
    if snapshot.account(&candidate.account_id).map(|a| a.kind) == Some(AccountKind::ApiKey) {
        return PaceFacts {
            remaining,
            reset,
            end: None,
            deadline: None,
            hours: None,
            score: remaining,
            window: None,
        };
    }
    let end = snapshot
        .term(&candidate.account_id)
        .and_then(|term| term::end_of(term, now));
    let deadline = match (reset, end) {
        (Some(r), Some(e)) => Some(r.min(e)),
        (Some(r), None) => Some(r),
        (None, Some(e)) => Some(e),
        (None, None) => None,
    };
    let hours = deadline
        .map(|d| ((d - now) as f64 / 3_600_000.0).max(1.0 / 60.0))
        .unwrap_or(WEEK_HOURS);
    PaceFacts {
        remaining,
        reset,
        end,
        deadline,
        hours: Some(hours),
        score: remaining / hours,
        window: Some(window.to_string()),
    }
}

/// `choose`'s sort key for a candidate: under the kind order, group one is oauth with plan quota, two oauth in
/// overage, three api_key. The split holds under any overage_policy, because it orders by cost, not eligibility:
/// `allow` still demotes an overage oauth account behind a plan-quota one. `overage_policy` governs eligibility
/// everywhere and orders the tier itself only when the kind order is off, through the overage bit. The kind order
/// puts the group ahead of the priority. Within a group, `prefer_plan` still orders an overage account behind a
/// plan-quota one, group three (api_key) included. A candidate step 6 kept on extra usage with its model week
/// spent (`spent_model`) bills as an account in overage does, so every plan-funded sibling goes ahead of it
/// whatever the priorities: it joins the overage group under the kind order and leads the key otherwise. The third
/// element is the overage bit under either order, which the selection trace reads.
fn tier_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    candidate: &Candidate,
    spent_model: &[String],
    now: i64,
    fallback_ms: i64,
) -> Tier {
    let spent = spent_model.contains(&candidate.account_id);
    let in_overage = state.in_overage(&candidate.account_id, now, fallback_ms);
    let overage_bit =
        u8::from(spent || (pool.overage_policy == OveragePolicy::PreferPlan && in_overage));
    if pool.kind_ordered() {
        let group = match (
            snapshot.account(&candidate.account_id).map(|a| a.kind),
            in_overage || spent,
        ) {
            (Some(AccountKind::ApiKey), _) => 2,
            (_, true) => 1,
            (_, false) => 0,
        };
        (group, candidate.priority, overage_bit)
    } else {
        (u8::from(spent), candidate.priority, overage_bit)
    }
}

/// `places` is false for a request the cap holds: the choice names the account it waits for and places nothing.
/// `spent_model` names the candidates step 6 kept on extra usage with the request's model week spent. The pick's
/// line names the tier, every tier member's score under Pace, the ties and the round-robin turn, and the tier's
/// members take their rank, the chosen one first.
#[allow(clippy::too_many_arguments)]
fn choose<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    set: &[&'a Candidate],
    spent_model: &[String],
    now: i64,
    fallback_ms: i64,
    places: bool,
    rng: &mut impl Rng,
    rec: &mut Recorder,
) -> Option<(&'a Candidate, Tier)> {
    let strategy = pool.strategy.as_str();
    let key_of = |c: &Candidate| tier_of(state, snapshot, pool, c, spent_model, now, fallback_ms);
    let Some(first) = set.iter().map(|c| key_of(c)).min() else {
        rec.step("pick", "none", vec![Fact::text("strategy", strategy)]);
        return None;
    };
    let tier: Vec<&Candidate> = set.iter().copied().filter(|c| key_of(c) == first).collect();
    let mut facts = vec![
        Fact::text("strategy", strategy),
        Fact::list(
            "tier",
            vec![
                first.0.to_string(),
                first.1.to_string(),
                first.2.to_string(),
            ],
        ),
    ];
    let mut scores: Vec<Score> = Vec::new();
    let chosen = match pool.strategy {
        Strategy::Pace => {
            let scored: Vec<(&Candidate, f64)> = tier
                .iter()
                .map(|c| (*c, pace_of(state, snapshot, c, window, now).score))
                .collect();
            let best = scored
                .iter()
                .map(|(_, score)| *score)
                .fold(f64::NEG_INFINITY, f64::max);
            let ties: Vec<&Candidate> = scored
                .iter()
                .filter(|(_, score)| *score == best)
                .map(|(c, _)| *c)
                .collect();
            for (c, score) in &scored {
                let unit = score_unit(snapshot, &c.account_id);
                facts.push(
                    Fact::number("score", *score)
                        .unit(unit)
                        .about(&c.account_id),
                );
                scores.push(Score {
                    account_id: c.account_id.clone(),
                    score: *score,
                    unit,
                });
            }
            facts.push(Fact::list(
                "ties",
                ties.iter().map(|c| c.account_id.clone()).collect(),
            ));
            facts.push(Fact::bool("drawn", ties.len() > 1));
            Some(ties[rng.random_range(0..ties.len())])
        }
        Strategy::Priority => {
            facts.push(Fact::list(
                "ties",
                tier.iter().map(|c| c.account_id.clone()).collect(),
            ));
            facts.push(Fact::bool("drawn", tier.len() > 1));
            Some(tier[rng.random_range(0..tier.len())])
        }
        Strategy::RoundRobin => {
            // The cursor walks the tier's sorted ids, so an account's turn does not depend on the order
            // `resolve` happened to build the candidate list in.
            let mut cohort: Vec<&str> = tier.iter().map(|c| c.account_id.as_str()).collect();
            cohort.sort_unstable();
            let i = state.round_robin_next(&pool.id, first, &cohort, places);
            facts.push(Fact::text("turn", format!("{} of {}", i + 1, cohort.len())));
            facts.push(Fact::bool("drawn", false));
            tier.iter().copied().find(|c| c.account_id == cohort[i])
        }
    };
    let Some(chosen) = chosen else {
        rec.step("pick", "none", facts);
        return None;
    };
    let score_of = |id: &str| scores.iter().find(|s| s.account_id == id).map(|s| s.score);
    let mut order: Vec<(&str, Option<f64>)> = tier
        .iter()
        .filter(|c| c.account_id != chosen.account_id)
        .map(|c| (c.account_id.as_str(), score_of(&c.account_id)))
        .collect();
    order.sort_by(|a, b| match (a.1, b.1) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        _ => Ordering::Equal,
    });
    for (rank, (id, score)) in
        std::iter::once((chosen.account_id.as_str(), score_of(&chosen.account_id)))
            .chain(order)
            .enumerate()
    {
        if let Some(member) = rec.candidate_mut(id) {
            member.rank = Some(rank as u32 + 1);
            member.score = score;
        }
    }
    rec.step("pick", "chosen", facts);
    rec.pick = Some(Pick {
        account_id: chosen.account_id.clone(),
        tier: first,
        scores,
    });
    Some((chosen, first))
}

/// The answer for a request nothing can serve, the pinned account's obstacle first when it has one. A fast rate
/// limit anywhere answers the earliest fast reset, since the client waits for the fast lane rather than for a
/// window it does not draw (`obstacle` names one only where it is the account's one blocker); a set whose every
/// timed obstacle is a credits block answers the first one's reason, which no wait clears before the poll reads
/// credits again.
fn exhaustion_of(obstacles: Vec<Obstacle>) -> Exhaustion {
    let fast_reset = obstacles
        .iter()
        .filter_map(|o| match o {
            Obstacle::Cooldown { reason, until, .. } if reason == FAST_RATE_LIMIT_REASON => {
                Some(*until)
            }
            _ => None,
        })
        .min();
    if let Some(deadline) = fast_reset {
        return Exhaustion::FastLane { deadline };
    }
    let credits: Vec<Option<&str>> = obstacles
        .iter()
        .filter_map(|o| match o {
            Obstacle::Cooldown { reason, window, .. } => {
                Some((reason == FAST_CREDITS_REASON).then_some(window.as_str()))
            }
            Obstacle::Credits { reason } => Some(Some(reason.as_str())),
            Obstacle::Budget { .. } | Obstacle::BudgetUnknown => Some(None),
            _ => None,
        })
        .collect();
    if let Some(Some(reason)) = credits.first() {
        if credits.iter().all(Option::is_some) {
            return Exhaustion::FastCredits {
                reason: reason.to_string(),
            };
        }
    }
    let cooldowns: Vec<(i64, String, String)> = obstacles
        .into_iter()
        .filter_map(|o| match o {
            Obstacle::Budget { until, .. } => Some((until, "overage".into(), "budget".into())),
            // A credits row's window is the upstream's refusal reason, a claim the client does not know; the
            // credits it refused are the overage the client does.
            Obstacle::Cooldown { until, reason, .. } if reason == FAST_CREDITS_REASON => {
                Some((until, "overage".into(), reason))
            }
            Obstacle::Cooldown {
                until,
                window,
                reason,
            } => Some((until, window, reason)),
            _ => None,
        })
        .collect();
    let timed = cooldowns
        .iter()
        .filter(|(_, _, reason)| reason != MODEL_REJECTED_REASON)
        .min_by_key(|(until, _, _)| *until);
    match timed {
        Some((deadline, window, _)) => Exhaustion::Timed {
            deadline: *deadline,
            window: window.clone(),
        },
        None if !cooldowns.is_empty() => Exhaustion::ModelRejected,
        None => Exhaustion::Untimed,
    }
}

fn kind_name(kind: Option<AccountKind>) -> &'static str {
    match kind {
        Some(AccountKind::Oauth) => "oauth",
        Some(AccountKind::ApiKey) => "api_key",
        None => "unknown",
    }
}

fn kind_fact(kind: Option<AccountKind>) -> Fact {
    Fact::text("kind", kind_name(kind))
}

fn account_fact(account_id: &str) -> Fact {
    Fact::text("account_id", account_id).about(account_id)
}

fn in_flight_fact(in_flight: u32, cap: u32) -> Fact {
    Fact::text("in_flight", format!("{in_flight}/{cap}")).unit("count")
}

/// Pace scores a subscription account per hour of its window and an api_key one by its remaining share alone.
fn score_unit(snapshot: &RegistrySnapshot, account_id: &str) -> &'static str {
    match snapshot.account(account_id).map(|a| a.kind) {
        Some(AccountKind::ApiKey) => "percent",
        _ => "percent_per_hour",
    }
}

fn pin_key_text(key: &PinKey) -> String {
    format!(
        "{}/{}/{}/{}",
        key.key_id, key.session_id, key.agent_id, key.model
    )
}

/// An obstacle as the exhausted line names it beside its account. The model-window partition's deadline is a
/// spent window rather than a cooldown row, so it reads as such.
fn weighed_name(account_id: &str, obstacle: &Obstacle) -> String {
    let name = match obstacle {
        Obstacle::Cooldown { reason, .. } if reason == "spent" => "spent",
        other => obstacle_name(other),
    };
    format!("{account_id}:{name}")
}

fn exhaustion_facts(exhaustion: &Exhaustion) -> ExhaustionFacts {
    let facts = |kind: &'static str| ExhaustionFacts {
        kind,
        deadline: None,
        window: None,
        reason: None,
        obstacle: None,
    };
    match exhaustion {
        Exhaustion::ModelRejected => facts("model_rejected"),
        Exhaustion::Timed { deadline, window } => ExhaustionFacts {
            deadline: Some(*deadline),
            window: Some(window.clone()),
            ..facts("timed")
        },
        Exhaustion::Untimed => facts("untimed"),
        Exhaustion::FastLane { deadline } => ExhaustionFacts {
            deadline: Some(*deadline),
            ..facts("fast_lane")
        },
        Exhaustion::FastCredits { reason } => ExhaustionFacts {
            reason: Some(reason.clone()),
            ..facts("fast_credits")
        },
        Exhaustion::Pinned { obstacle } => ExhaustionFacts {
            obstacle: Some(obstacle),
            ..facts("pinned")
        },
    }
}

fn exhausted_facts(exhaustion: &Exhaustion, obstacles: Vec<String>) -> Vec<Fact> {
    let answer = exhaustion_facts(exhaustion);
    let mut facts = vec![Fact::text("kind", answer.kind)];
    if let Some(deadline) = answer.deadline {
        facts.push(Fact::int("deadline", deadline).unit("time"));
    }
    if let Some(window) = answer.window {
        facts.push(Fact::text("window", window));
    }
    if let Some(reason) = answer.reason {
        facts.push(Fact::text("reason", reason));
    }
    if let Some(obstacle) = answer.obstacle {
        facts.push(Fact::text("obstacle", obstacle));
    }
    facts.push(Fact::list("obstacles", obstacles));
    facts
}

/// Step 1's reading of the pin row, `row` `None` when the session holds none under this key and agent.
fn pin_facts(found: &'static str, row: Option<&PinRow>, marked: bool, now: i64) -> PinFacts {
    PinFacts {
        found,
        account_id: row.map(|p| p.account_id.clone()),
        epoch: row.map(|p| p.epoch),
        incarnation: row.map(|p| p.incarnation),
        created_at: row.map(|p| p.created_at),
        last_used_at: row.map(|p| p.last_used_at),
        last_refresh_at: row.map(|p| p.last_refresh_at),
        idle_ms: row.map(|p| now - p.last_used_at),
        refresh_age_ms: row.map(|p| now - p.last_refresh_at),
        lane: row.map(|p| p.lane.clone()),
        shed: row
            .and_then(|p| p.shed_from.clone().zip(p.shed_at))
            .map(|(from, at)| PinShed { from, at }),
        marked,
        warmth: None,
        warmth_cause: None,
        released: Vec::new(),
    }
}

fn pin_line(pin: &PinFacts) -> Vec<Fact> {
    let mut line = Vec::with_capacity(8);
    line.push(Fact::text("found", pin.found));
    if let Some(account_id) = &pin.account_id {
        line.push(account_fact(account_id));
    }
    if let Some(epoch) = pin.epoch {
        line.push(Fact::count("epoch", epoch));
    }
    if let Some(idle) = pin.idle_ms {
        line.push(Fact::int("idle_ms", idle).unit("ms"));
    }
    if let Some(age) = pin.refresh_age_ms {
        line.push(Fact::int("refresh_age_ms", age).unit("ms"));
    }
    if let Some(warmth) = pin.warmth {
        line.push(Fact::text("warmth", warmth));
    }
    if let Some(cause) = pin.warmth_cause {
        line.push(Fact::text("cause", cause));
    }
    if pin.found == "live" {
        line.push(Fact::bool("marked", pin.marked));
    }
    line
}

fn window_facts(reading: &WindowReading, ahead: Option<f64>, now: i64) -> WindowFacts {
    WindowFacts {
        utilization: reading.utilization,
        reset: reading.reset,
        source: reading.source.as_str(),
        observed_at: reading.observed_at,
        age_ms: now - reading.observed_at,
        ahead,
    }
}

fn obstacle_facts(obstacle: &Obstacle) -> ObstacleFacts {
    let (reason, window, until) = match obstacle {
        Obstacle::Cooldown {
            reason,
            until,
            window,
        } => (Some(reason.clone()), Some(window.clone()), Some(*until)),
        Obstacle::Budget { until, period } => {
            (Some(period.as_str().to_string()), None, Some(*until))
        }
        Obstacle::Credits { reason } => (Some(reason.clone()), None, None),
        Obstacle::Attempted | Obstacle::BudgetUnknown | Obstacle::Ended | Obstacle::Operator => {
            (None, None, None)
        }
    };
    ObstacleFacts {
        name: obstacle_name(obstacle),
        reason,
        window,
        until,
    }
}

/// A member's standing as step 2 reads it, eligible or not: the readings the rules and the pick below weigh. The
/// tier is the one step 2 can know, with no member kept on extra usage yet. The partition corrects it.
#[allow(clippy::too_many_arguments)]
fn candidate_facts(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    candidate: &Candidate,
    window: &str,
    obstacle: Option<&Obstacle>,
) -> CandidateFacts {
    let (now, id) = (request.now, candidate.account_id.as_str());
    let account = snapshot.account(id);
    let kind = account.map(|a| a.kind);
    let sample = state.sample(id);
    let mut windows = BTreeMap::new();
    if kind != Some(AccountKind::ApiKey) {
        for name in [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE] {
            if let Some(reading) = state.predicted(id, name, now) {
                let ahead = (name != SEVEN_DAY_FABLE)
                    .then(|| reading.utilization - elapsed_share(&reading, name, now));
                windows.insert(name.to_string(), window_facts(&reading, ahead, now));
            }
        }
    }
    if let Some(reading) = sample.and_then(|s| s.worst_bucket(now)) {
        windows.insert(
            "worst_bucket".to_string(),
            window_facts(&reading, None, now),
        );
    }
    for name in [FAST_INPUT, FAST_OUTPUT] {
        if let Some(reading) = sample.and_then(|s| s.reading(name)) {
            windows.insert(name.to_string(), window_facts(&reading, None, now));
        }
    }
    let budget = snapshot
        .members(&pool.id)
        .iter()
        .find(|m| m.account_id == id)
        .map(|member| {
            let spends: Vec<&MemberSpend> = state.member_spends_of(&pool.id, id).collect();
            member
                .budgets()
                .map(|(period, budget_usd)| {
                    let spend = spends.iter().find(|s| s.period == period);
                    BudgetFacts {
                        period: period.as_str(),
                        budget_usd,
                        spent_usd: spend.map(|s| s.spent_usd),
                        period_start: spend.map(|s| s.period_start),
                        period_end: spend.map(|s| s.period_end),
                        observed_at: spend.map(|s| s.observed_at),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let blocking = state.blocking_cooldown(id, &request.canonical_model, now, request.fast);
    let mut cooldowns: Vec<CooldownFacts> = state
        .cooldowns_of(id, now)
        .map(|row| CooldownFacts {
            scope: row.scope.clone(),
            reason: row.reason.clone(),
            window: row.window.clone(),
            until: row.until,
            blocking: blocking.is_some_and(|b| b.scope == row.scope),
        })
        .collect();
    cooldowns.sort_by(|a, b| a.scope.cmp(&b.scope));
    let credits = account
        .filter(|_| request.fast)
        .and_then(|a| state.credits_of(id, a.incarnation))
        .map(|c| CreditsFacts {
            enabled: c.enabled,
            disabled_reason: c.disabled_reason.clone(),
            utilization: c.utilization,
            observed_at: c.observed_at,
            blocked: c.blocked().map(String::from),
        });
    let term = snapshot.term(id).map(|t| TermFacts {
        since: t.since,
        trial_ends_at: t.trial_ends_at,
        cancelled: t.cancelled,
        ends_at: t.ends_at,
        winding_down: term::winding_down(t, now, pool.failover.wind_down_minutes),
        ended: term::ended(t, now),
    });
    CandidateFacts {
        account_id: id.to_string(),
        incarnation: candidate.incarnation,
        kind: kind_name(kind),
        plan_tier: snapshot.tier(id).map(String::from),
        enabled: account.is_some_and(|a| a.enabled),
        state: account.map_or("deleted", |a| state_name(a.state).unwrap_or("ready")),
        priority: candidate.priority,
        tier: tier_of(
            state,
            snapshot,
            pool,
            candidate,
            &[],
            now,
            request.fallback_ms,
        ),
        in_overage: state.in_overage(id, now, request.fallback_ms),
        overage: state
            .overage(id)
            .filter(|e| e.in_use)
            .map(|e| OverageFacts {
                since: e.since.unwrap_or(e.observed_at),
                reset: e.reset,
                source: e.source.as_str(),
                observed_at: e.observed_at,
            }),
        in_flight: state.in_flight().account(id),
        in_flight_cap: pool.failover.in_flight_cap,
        windows,
        allowance_known: allowance_known(state, snapshot, id, window, now),
        pace: (pool.strategy == Strategy::Pace)
            .then(|| pace_of(state, snapshot, candidate, window, now)),
        budget,
        cooldowns,
        credits,
        term,
        prefix: PrefixLookup {
            content: None,
            content_match: None,
            static_: None,
            static_match: None,
            with_main: false,
        },
        obstacle: obstacle.map(obstacle_facts),
        partition: None,
        rank: None,
        score: None,
        verdict: if obstacle.is_some() {
            "out"
        } else {
            "eligible"
        },
    }
}

fn request_facts(request: &SelectRequest, pool_id: String) -> RequestFacts {
    RequestFacts {
        request_id: request.request_id.clone(),
        attempt: request.attempt,
        seq: request.seq,
        at: request.now,
        key_id: request.key_id.clone(),
        key_generation: request.key_generation,
        pool_id,
        agent_id: request.agent_id.clone(),
        parent_agent_id: request.parent_agent_id.clone(),
        client_class: request.client.as_str(),
        request_class: request.request_class.clone(),
        canonical_model: request.canonical_model.clone(),
        lane: LaneFacts::of(&request.lane),
        flags: RequestFlags::of(request),
        chain: ChainFacts::of(request),
        attempted: request.attempted.clone(),
        holds: request.holds.clone(),
        registry_revision: request.revision,
    }
}

/// The record and its summary from the answer and what the steps recorded: the outcome, every member's verdict,
/// and the row's `by` and `fact`, with the shedding round the decision ran, which the record keeps the marks of.
fn finish(
    rec: Recorder,
    request: &SelectRequest,
    snapshot: &RegistrySnapshot,
    pool: Option<&PoolRow>,
    result: &SelectResult,
) -> (DecisionRecord, DecisionSummary, Option<ShedRound>) {
    let Recorder {
        walk,
        mut candidates,
        pin,
        pin_reason,
        rule,
        source,
        destinations,
        pin_written,
        hold_place,
        shed_round,
        pick,
        renewal,
    } = rec;
    let key = snapshot.key(&request.key_id);
    let settings = pool.zip(key).map(|(pool, _)| settings_of(pool));
    let mut outcome = Outcome {
        kind: "placed",
        account_id: None,
        from_account_id: None,
        upstream_model: None,
        tier: None,
        strategy: pool.map_or("", |p| p.strategy.as_str()),
        rule: rule.map(MigrateReason::as_str),
        rung: None,
        in_overage: false,
        pin_written,
        pin_events: Vec::new(),
        hold: None,
        exhaustion: None,
        refusal: None,
        cohort_hold: cohort_hold(&request.holds),
        shed_marks: shed_round.as_ref().map(|round| round.facts.marks.clone()),
    };
    let mut skipped = 0;
    match result {
        SelectResult::Selected(selection) => {
            outcome.kind = if selection.trace.renewed {
                "renewed"
            } else if selection.migrated.is_some() {
                "moved"
            } else {
                "placed"
            };
            outcome.account_id = Some(selection.account.id.clone());
            outcome.from_account_id = source.clone();
            outcome.upstream_model = Some(selection.upstream_model.clone());
            outcome.tier = selection.trace.tier;
            outcome.strategy = selection.trace.strategy;
            outcome.rung = selection.trace.placement.clone();
            outcome.in_overage = selection.in_overage;
            outcome.cohort_hold = selection.trace.cohort_hold.clone();
            skipped = u32::try_from(selection.trace.skipped.len()).unwrap_or(u32::MAX);
        }
        SelectResult::Held { account_id, hold } => {
            outcome.kind = "held";
            outcome.account_id = Some(account_id.clone());
            // The pick's tier is the hold's only when the hold waits on the pick's account, whatever placed the
            // hold there, as `scores` keys the pick's score.
            outcome.tier = pick
                .as_ref()
                .filter(|pick| &pick.account_id == account_id)
                .map(|pick| pick.tier);
            outcome.hold = Some(HoldOutcome {
                account_id: hold.account_id.clone(),
                cohort: hold.cohort.clone(),
                bound_ms: pool.map_or(0, |p| seconds_ms(p.failover.retry_in_place_seconds)),
            });
        }
        SelectResult::Exhausted(exhaustion) => {
            outcome.kind = "exhausted";
            outcome.exhaustion = Some(exhaustion_facts(exhaustion));
        }
        SelectResult::Refused(refusal) => {
            outcome.kind = "refused";
            outcome.refusal = Some(RefusalFacts {
                kind: "key",
                reason: match refusal {
                    Refusal::Key(refusal) => refusal.to_string(),
                    Refusal::Resolve(refusal) => refusal.to_string(),
                },
            });
        }
    }
    let account = outcome.account_id.as_deref();
    for member in &mut candidates {
        let id = member.account_id.as_str();
        member.verdict = if Some(id) == account {
            match outcome.kind {
                "renewed" => "renewed",
                "held" => "held_on",
                _ => "chosen",
            }
        } else if source.as_deref() == Some(id) {
            "source"
        } else if destinations.iter().any(|d| d == id) {
            "destination"
        } else if member.obstacle.is_some() || member.partition.is_some() {
            "out"
        } else if member.rank.is_some() {
            "ranked"
        } else if pick.is_some() {
            "eligible"
        } else {
            "not_needed"
        };
    }
    let read = Readings {
        outcome: &outcome,
        request,
        pool,
        candidates: &candidates,
        walk: &walk,
        pin: pin.as_ref(),
        renewal: renewal.as_ref(),
        hold_place,
        pick: pick.as_ref(),
    };
    let (by, fact) = read.by_and_fact();
    let (score, runner_up) = read.scores();
    let summary = DecisionSummary {
        kind: outcome.kind,
        probe: request.probe,
        by,
        fact,
        rule: outcome.rule,
        strategy: outcome.strategy,
        tier: outcome.tier,
        rung: outcome.rung.clone(),
        score,
        runner_up,
        skipped,
        warmth: pin.as_ref().and_then(|p| {
            Some(WarmthFacts {
                verdict: p.warmth?,
                cause: p.warmth_cause?,
            })
        }),
        hold: outcome.hold.clone(),
        exhaustion: outcome.exhaustion.clone(),
        refusal: outcome.refusal.clone(),
        fast: request.fast,
    };
    let pool_id = pool
        .map(|p| p.id.clone())
        .or_else(|| key.map(|k| k.pool_id.clone()))
        .unwrap_or_default();
    let record = DecisionRecord {
        version: RECORD_VERSION,
        request: request_facts(request, pool_id),
        pin,
        pin_reason,
        settings,
        candidates,
        walk,
        outcome,
    };
    (record, summary, shed_round)
}

/// What the summary's `by` and `fact` are computed from: the outcome and the facts the steps recorded, never a
/// fresh read of the state.
struct Readings<'r> {
    outcome: &'r Outcome,
    request: &'r SelectRequest,
    pool: Option<&'r PoolRow>,
    candidates: &'r [CandidateFacts],
    walk: &'r [WalkStep],
    pin: Option<&'r PinFacts>,
    renewal: Option<&'r Renewal>,
    hold_place: Option<&'static str>,
    pick: Option<&'r Pick>,
}

fn summary_fact(key: &'static str, value: FactValue, unit: &'static str) -> SummaryFact {
    SummaryFact {
        key,
        value,
        unit: Some(unit),
        entity: None,
    }
}

impl Readings<'_> {
    fn member(&self, account_id: &str) -> Option<&CandidateFacts> {
        self.candidates.iter().find(|c| c.account_id == account_id)
    }

    fn by_and_fact(&self) -> (By, Vec<SummaryFact>) {
        let by = |kind: &'static str, value: &str| By {
            kind,
            value: value.to_string(),
            tier: None,
        };
        let outcome = self.outcome;
        match outcome.kind {
            "renewed" => (by("none", ""), self.renewal_fact()),
            "moved" => {
                // A move continues as a fresh placement: its By says why it left, its Fact where it landed.
                let none_cool = self
                    .walk
                    .iter()
                    .any(|w| w.step == "cool" && w.verdict == "fell_through");
                let fact = if none_cool {
                    vec![summary_fact(
                        "pick",
                        FactValue::Text("none cool".into()),
                        "text",
                    )]
                } else {
                    self.placement_fact()
                };
                (by("rule", outcome.rule.unwrap_or_default()), fact)
            }
            // The slot the cap's exemption found explains the landing, so it leads. The wait, when there was one,
            // says what the landing cost.
            "placed" if self.hold_place == Some("session_pin") => {
                (by("hold", "session_pin"), self.session_pin_fact())
            }
            "placed" => match self.attempt_hold() {
                // The wait explains the placement more than where it landed: the row names the hold it followed
                // and how long it lasted.
                Some(hold) => {
                    let place = if hold.cohort.is_some() {
                        "cohort"
                    } else {
                        "strategy_pick"
                    };
                    let held = i64::try_from(hold.held_ms).unwrap_or(i64::MAX);
                    (
                        by("hold", place),
                        vec![summary_fact("held", FactValue::Int(held), "ms")],
                    )
                }
                None => {
                    let by = match &outcome.rung {
                        Some(rung) => by("rung", rung.rung),
                        None => By {
                            kind: "strategy",
                            value: outcome.strategy.to_string(),
                            tier: outcome.tier,
                        },
                    };
                    (by, self.placement_fact())
                }
            },
            "held" => (
                by("hold", self.hold_place.unwrap_or("strategy_pick")),
                self.in_flight_fact(),
            ),
            "exhausted" => {
                let Some(exhaustion) = &outcome.exhaustion else {
                    return (by("exhaustion", ""), Vec::new());
                };
                let fact = match (&exhaustion.reason, exhaustion.deadline) {
                    (Some(reason), _) => {
                        summary_fact("cause", FactValue::Text(reason.clone()), "text")
                    }
                    (None, Some(deadline)) => {
                        summary_fact("retry", FactValue::Int(deadline), "time")
                    }
                    (None, None) => summary_fact("retry", FactValue::Text("none".into()), "text"),
                };
                (by("exhaustion", exhaustion.kind), vec![fact])
            }
            _ => {
                let kind = outcome.refusal.as_ref().map_or("", |r| r.kind);
                (by("refusal", kind), vec![key_fact(&self.request.key_id)])
            }
        }
    }

    /// A probe's renewal names the pin's epoch. Any other names the warmth's cause when the pin was cold, else the
    /// hottest shared window at or above the shed threshold, else the pin's idle age.
    fn renewal_fact(&self) -> Vec<SummaryFact> {
        let Some(renewal) = self.renewal else {
            return Vec::new();
        };
        let cause = || {
            renewal
                .cause
                .map(|cause| vec![summary_fact("cause", FactValue::Text(cause.into()), "text")])
        };
        if self.request.probe {
            let epoch = i64::try_from(renewal.epoch).unwrap_or(i64::MAX);
            return vec![summary_fact("epoch", FactValue::Int(epoch), "count")];
        }
        if renewal.cold {
            if let Some(fact) = cause() {
                return fact;
            }
        }
        let threshold = self.pool.and_then(|p| p.shed_ahead_of_pace);
        let member = self
            .outcome
            .account_id
            .as_deref()
            .and_then(|id| self.member(id));
        if let (Some(threshold), Some(member)) = (threshold, member) {
            let hottest = [FIVE_HOUR, SEVEN_DAY]
                .into_iter()
                .filter_map(|name| {
                    let window = member.windows.get(name)?;
                    let lead = window.ahead.filter(|lead| *lead >= threshold)?;
                    Some((name, lead, window.utilization))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((name, _, utilization)) = hottest {
                return vec![summary_fact(
                    name,
                    FactValue::Number(utilization),
                    "percent",
                )];
            }
        }
        self.pin
            .and_then(|p| p.idle_ms)
            .map(|idle| vec![summary_fact("idle", FactValue::Int(idle), "ms")])
            .or_else(cause)
            .unwrap_or_default()
    }

    /// Where a placement landed and why there: the rung's matched prefix, or the pick's contest.
    fn placement_fact(&self) -> Vec<SummaryFact> {
        match &self.outcome.rung {
            Some(rung) if rung.rung == "with_main" => Vec::new(),
            Some(rung) => vec![
                summary_fact(
                    "prefix",
                    FactValue::Int(i64::try_from(rung.prefix_tokens).unwrap_or(i64::MAX)),
                    "tokens",
                ),
                summary_fact("depth", FactValue::Int(i64::from(rung.depth)), "count"),
            ],
            None => self.pick_fact(),
        }
    }

    fn pick_fact(&self) -> Vec<SummaryFact> {
        let text =
            |key: &'static str, value: String| summary_fact(key, FactValue::Text(value), "text");
        // Read off the accounts tried rather than the wire attempt: a retry in place advances the attempt on one
        // account, and a credential failure moves on to the next without advancing it.
        let chosen = self.outcome.account_id.as_deref();
        let mut others = self
            .candidates
            .iter()
            .filter(|c| Some(c.account_id.as_str()) != chosen)
            .peekable();
        if others.peek().is_some() && others.all(|c| self.request.attempted.contains(&c.account_id))
        {
            return vec![text("pick", "last account".into())];
        }
        let tier_size = self.candidates.iter().filter(|c| c.rank.is_some()).count();
        if tier_size <= 1 {
            return vec![text("pick", "only candidate".into())];
        }
        match self.outcome.strategy {
            "pace" => {
                let (Some(score), Some(runner_up)) = self.scores() else {
                    return Vec::new();
                };
                vec![
                    summary_fact(
                        "gap",
                        FactValue::Number(score.score - runner_up.score),
                        score.unit,
                    ),
                    SummaryFact {
                        key: "over",
                        value: FactValue::Text(runner_up.account_id.clone()),
                        unit: None,
                        entity: Some(FactEntity {
                            kind: "account",
                            id: runner_up.account_id,
                        }),
                    },
                ]
            }
            "round_robin" => self
                .walk
                .iter()
                .filter(|w| w.step == "pick")
                .flat_map(|w| &w.facts)
                .find(|f| f.key == "turn")
                .map(|f| summary_fact("turn", f.value.clone(), "text"))
                .into_iter()
                .collect(),
            _ => vec![text("rank", format!("1 of {tier_size}"))],
        }
    }

    fn in_flight_fact(&self) -> Vec<SummaryFact> {
        self.outcome
            .account_id
            .as_deref()
            .and_then(|id| self.member(id))
            .map(|m| {
                vec![summary_fact(
                    "in_flight",
                    FactValue::Text(format!("{}/{}", m.in_flight, m.in_flight_cap)),
                    "text",
                )]
            })
            .unwrap_or_default()
    }

    /// The hold this attempt waited through, if the request's last hold is this attempt's. A failover's later
    /// attempt took its own placement and reads `None`. Both arms that name a wait read it here.
    fn attempt_hold(&self) -> Option<&HoldFacts> {
        self.request
            .holds
            .last()
            .filter(|hold| hold.attempt == self.request.attempt)
    }

    /// A placement on the session's pin: `in_flight` as a hold's, then `held` with the wait when `attempt_hold`
    /// answers one.
    fn session_pin_fact(&self) -> Vec<SummaryFact> {
        let mut fact = self.in_flight_fact();
        if let Some(hold) = self.attempt_hold() {
            let held = i64::try_from(hold.held_ms).unwrap_or(i64::MAX);
            fact.push(summary_fact("held", FactValue::Int(held), "ms"));
        }
        fact
    }

    /// The pick's winning Pace score and the best of the rest of its tier. `None` without a Pace pick, and when the
    /// row landed elsewhere than the pick (a session's pin, a cohort's account), whose standing the scores are not.
    fn scores(&self) -> (Option<Score>, Option<Score>) {
        let Some(Pick {
            account_id: chosen,
            scores,
            ..
        }) = self.pick
        else {
            return (None, None);
        };
        if self.outcome.account_id.as_ref() != Some(chosen) {
            return (None, None);
        }
        let score = scores.iter().find(|s| &s.account_id == chosen).cloned();
        let runner_up = scores
            .iter()
            .filter(|s| &s.account_id != chosen)
            .max_by(|a, b| a.score.total_cmp(&b.score))
            .cloned();
        (score, runner_up)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::Recorder;

    #[test]
    #[should_panic(expected = "the walk step x/match is not in WALK_STEPS")]
    fn the_recorder_refuses_a_step_id_the_table_does_not_list() {
        Recorder::default().step("x", "match", Vec::new());
    }

    #[test]
    #[should_panic(expected = "the walk step pin/fired is not in WALK_STEPS")]
    fn the_recorder_refuses_a_verdict_the_table_does_not_list_for_its_step() {
        Recorder::default().step("pin", "fired", Vec::new());
    }
}
