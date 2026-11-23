//! One selection: the spec's seven steps over the snapshot and the routing state.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use phoebis_store::{
    AccountKind, AccountRow, AccountState, AffinityMode, BudgetPeriod, CooldownRow, CooldownScope,
    OveragePolicy, PinBurn, PinKey, PinMark, PinRow, PoolMember, PoolRow, ReturnToPlan,
    SessionPlacement, ShedStamp, Strategy, SubagentPlacement, ThinkingSum, WarmSessions,
};
use rand::{Rng, RngExt};

use crate::affinity::{
    AffinityEntry, AffinityMap, RungWalk, Visited, FIVE_MINUTES_MS, ONE_HOUR_MS,
};
use crate::record::{
    key_fact, settings_of, BudgetFacts, By, CandidateFacts, CooldownFacts, CreditsFacts,
    DecisionRecord, DecisionSummary, ExhaustionFacts, Fact, FactEntity, FactValue, HoldOutcome,
    MarkClearedFacts, MoveCause, ObstacleFacts, Outcome, OverageFacts, PaceFacts, PinFacts,
    PinKeyFacts, PinShed, PinWritten, PrefixEntry, PrefixLookup, RefusalFacts, RefusedRequest,
    RequestFacts, ReservationFacts, Score, ShedPinFacts, ShedRoundFacts, SummaryFact, TermFacts,
    WalkStep, WarmthFacts, WindowFacts, RECORD_VERSION, WALK_STEPS,
};
use crate::state::{
    all_plan_windows, budget_reached, burn_average_ms, burn_decayed, ceiling, keeps_off,
    load_shedding, mark_holds, plan_windows, reservation_shedding, return_to_plan_dwell_ms,
    round_in_dwell, shed_cap, shed_dwell_ms, slow_down, slow_down_hold_ms, slow_down_stretch_ms,
    standing_mark, warm, BandSegment, BurnUnit, MemberSpend, PinPace, PinRef, PinUsage,
    Reservation, RoutingState, StateWrite, Tier, WindowReading, FAST_INPUT, FAST_OUTPUT, FIVE_HOUR,
    HOUR_MS, IN_FLIGHT_RECLAIM_MS, SEVEN_DAY, SEVEN_DAY_FABLE, WEEK_MS,
};
use crate::windows::{model_week, window_for, window_length_ms, MODEL_WINDOWS};
use crate::{paid_source, term, Candidate, KeyRefusal, RegistrySnapshot, ResolveError};

/// Who sent the request, as the record and a refused request carry it: `claude-code/*` is Claude Code,
/// everything else is not.
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
    /// The client's own session id, which the contract guarantees of every request the relay selects for: the
    /// pin and the pin events are keyed on it.
    pub session_id: String,
    pub canonical_model: String,
    /// The client's spelling with the `[1m]` suffix removed and ASCII lowercased, the relay's own folded id, so
    /// the stale-revision re-resolve keeps the client's spelling rather than the canonical id, also when the
    /// admission resolved no candidate to copy it from.
    pub folded_model: String,
    pub candidates: Vec<Candidate>,
    pub portable: bool,
    /// The account that issued the fallback credit token the body carries, as the relay's store remembers it, `None`
    /// for a body without a token or a token the store does not hold.
    pub credit_account: Option<CreditAccount>,
    pub attempted: Vec<String>,
    pub now: i64,
    /// When the relay received the request, the same on every selection of it: a pin's cycle is read from its
    /// arrivals, so a second selection after a hold must not read its own instant as a fresh arrival.
    pub received_at: i64,
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
    /// Every boundary of the message chain, lowercase hex, empty when the body carried no chain. Shared with the
    /// attempts before and after and with the answer's affinity report, never copied per attempt.
    pub boundaries: Arc<[String]>,
    pub client: ClientClass,
    /// The user agent's entrypoint token, `None` for a client that is not Claude Code: the regime the request and the
    /// pin it places or renews price under.
    pub entrypoint: Option<String>,
    /// Every static boundary in chain order, lowercase hex, the last one the static hash.
    pub static_boundaries: Arc<[String]>,
    /// The body carries a `cache_control` marker, or the TTL policy will add one: the cache gate.
    pub marker: bool,
    /// `x-claude-code-parent-agent-id` as sent; `with_main` reads it, `main` when absent.
    pub parent_agent_id: Option<String>,
    /// The holds this request already waited through, oldest first: the relay carries each `Held` answer's
    /// facts into the next selection, so the attempt that follows a hold names it.
    pub holds: Vec<HoldFacts>,
    /// The pin a failover took off its account inside the migration window, or the pin of an account the registry no
    /// longer holds while its key stands under the generation the pin names and the pool's affinity TTL still holds
    /// its last use, which the routing task hands to the selection that lands the session again, `None` for every
    /// other request.
    pub failed_over: Option<FailedOver>,
    /// Whether the request's pin carried `ended` when the routing task took the request, read ahead of the clear that
    /// the agent's own request makes, or carried by a hold of this request. The routing task sets it, as it sets
    /// `failed_over`, and each other caller sends false.
    pub pin_ended: bool,
    /// Whether a leg of this request was already dispatched, as the relay reads it: true once a selection placed a
    /// leg, whether or not that leg reached the wire. `attempted` cannot stand in for it, since an account whose hold
    /// ran out, or one passed over at its cap, is attempted with nothing dialed.
    pub dialed: bool,
    /// Whether the relay passed an account over at its cap for this request: a hold whose bound ran out, an account
    /// it already held for once, or one met past the request's one bound. The relay answers the untimed exhaustion
    /// that follows from this reading, so the row records the same.
    pub held_out: bool,
    /// The cache lane the request writes to, as the relay composed it; a pin or an entry of another lane is cold.
    pub lane: String,
    /// The body carried `speed: "fast"`: fast eligibility applies.
    pub fast: bool,
    /// The request is the client's own classifier call: it books nothing, its body prices no rewrite and it takes the
    /// `none` verdict, as a fast request does.
    pub classifier: bool,
    /// The relay's estimate of the body's prompt tokens from its bytes (canonical bytes over four, the byte
    /// estimate the static affinity entries carry): a fresh session's rewrite, since it has no pin to read one from.
    pub body_tokens: u64,
    /// The effective effort of the request as the relay read it (the newest per-turn control entry over the
    /// top-level `output_config.effort`), the word as the client sent it, `None` where the request named none.
    pub effort: Option<String>,
}

impl SelectRequest {
    /// The static chain's last boundary, `None` when the body carried no static chain.
    pub fn static_hash(&self) -> Option<&String> {
        self.static_boundaries.last()
    }

    /// The key of the pin the request's session holds for its agent and model.
    pub(crate) fn pin_key(&self) -> PinKey {
        PinKey::new(
            &self.key_id,
            &self.session_id,
            &self.agent_id,
            &self.canonical_model,
        )
    }
}

/// The account that issued a fallback credit token, under the incarnation that answered the refusal: a holder created
/// again under the same id is another account, which the credit does not reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreditAccount {
    pub account_id: String,
    pub incarnation: u64,
}

/// The pin a failover took off its account, or the reload's purge off a deleted account, handed to the selection that
/// lands the session again.
#[derive(Clone, Debug, PartialEq)]
pub struct FailedOver {
    pub from_account_id: String,
    /// `cooldown`, `fast_lane` or `mark` for a failover, `deleted` for a deleted account's pin, mirroring the word of
    /// the invalidated entry the landing reads. The selection names it as the landing's move and rule, and the event
    /// takes its own from that entry.
    pub reason: &'static str,
    /// The row the invalidation took, whose burn the landing prices and the new pin carries.
    pub row: PinRow,
    pub cause: Option<MoveCause>,
}

/// The pin row's `shed_reason` for a shedding round's move, the word a stamp from before the row carried one reads as.
const SHED_ROUND: &str = "round";

/// The cache lane key, `<model>|<speed>|<thinking>|<effort>`: the one spelling the relay composes and every pin,
/// affinity entry and request compares, so no reader spells it twice.
pub fn lane_key(model: &str, speed: &str, thinking: &str, effort: &str) -> String {
    format!("{model}|{speed}|{thinking}|{effort}")
}

/// The parts of a lane `lane_key` spells, from the end, where the lane's own three sit: the effort, the thinking, the
/// speed, then the model, each `None` for a lane too short to hold it.
pub(crate) fn lane_parts(lane: &str) -> [Option<&str>; 4] {
    let mut parts = lane.rsplitn(4, '|');
    [parts.next(), parts.next(), parts.next(), parts.next()]
}

/// `lane` with its model part, the one ahead of its last three, replaced by what `model` answers for it. A lane too
/// short to hold a model comes back as it was.
pub(crate) fn lane_with_model<'a>(lane: &'a str, model: impl FnOnce(&'a str) -> &'a str) -> String {
    match lane_parts(lane)[3] {
        Some(part) => format!("{}{}", model(part), &lane[part.len()..]),
        None => lane.to_string(),
    }
}

/// The dated id `model` resolved to in `resolutions`, else `model` itself.
pub(crate) fn resolved<'a>(resolutions: &'a HashMap<String, String>, model: &'a str) -> &'a str {
    resolutions.get(model).map(String::as_str).unwrap_or(model)
}

/// Whether a pin or an entry stored on `stored` holds the cache a request on `lane` writes to. A row written
/// before rows carried a lane stores none and adopts the request's, since it never recorded another cache.
pub(crate) fn same_lane(stored: &str, lane: &str) -> bool {
    stored.is_empty() || stored == lane
}

/// Whether `lane` is a fast one: its speed part, the second of the three after the model, reads `fast`.
pub(crate) fn lane_is_fast(lane: &str) -> bool {
    lane_parts(lane)[2] == Some("fast")
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

phoebis_store::text_enum! {
    plain
    /// Why a request waits before its next selection. The record and the trace write its snake-case word, the word the
    /// routing log and the ledger read.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum HoldReason {
        Cap => "cap",
        Reset => "reset",
        Rewrite => "rewrite",
        SlowDown => "slow_down",
    }
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
    /// The wire attempt that waited, kept for the record's readers.
    pub attempt: u32,
    /// The decision that held, the request's `seq` when the selector answered the hold: the placement one `seq`
    /// past it is the one the wait led to, and a leg that failed over after the wait took a selection of its own.
    pub seq: u32,
    /// Whether the wait's bound ran out with no slot freed, which the relay learns when the wait ends.
    pub ran_out: bool,
    /// `cap` for a hold on a slot, `reset` for a spent window about to reset, `slow_down` for a slowed pin's wait
    /// before its head, `rewrite` for a moved pin's wait for the head of the request that carried the move.
    pub reason: HoldReason,
    /// The instant the request waits for: under `reset` the later of the spent window's reset and the end of the
    /// account's cooldown, under `slow_down` the pin's deadline, under `rewrite` the first request's dispatch plus the
    /// pool's hold cap. Under `cap` the admission, which the relay fills once a slot freed, and `None` on the hold the
    /// selection answers and where the wait ran out.
    pub until: Option<i64>,
    /// The request whose head the hold waits for. `Some` under `rewrite`, `None` under the other reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waits_for: Option<String>,
    /// Whether the held request's pin carried `ended` ahead of the routing task's clear. The relay carries it into the
    /// selection behind the wait, whose row no longer shows it. The serializer skips it, so no stored record holds it.
    #[serde(skip)]
    pub pin_ended: bool,
}

/// One wait of a request, oldest first: `cap`, `reset`, `slow_down` or `rewrite` and how long it lasted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HoldWait {
    pub reason: HoldReason,
    pub held_ms: u64,
}

/// The holds before this attempt, as the trace records them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CohortHold {
    pub waited_on: Vec<String>,
    pub cohort: Option<String>,
    /// The holds' waits summed: the time the request spent waiting, for a slot, a reset, a slow-down or a rewrite in
    /// flight.
    pub held_ms: u64,
    /// Each wait apart, oldest first, so the request row tells a slot wait, a reset wait, a slow-down hold and a wait
    /// for a rewrite in flight apart.
    pub waits: Vec<HoldWait>,
}

/// The summary of the holds a request waited through, `None` for none: the accounts waited on in order, the last
/// hold's cohort, the waits summed and each wait with its reason. The selector's trace and the relay's completion
/// both read it, so the attempt's entry and the request row never disagree.
pub fn cohort_hold(holds: &[HoldFacts]) -> Option<CohortHold> {
    let last = holds.last()?;
    Some(CohortHold {
        waited_on: holds.iter().map(|h| h.account_id.clone()).collect(),
        cohort: last.cohort.clone(),
        held_ms: holds.iter().map(|h| h.held_ms).sum(),
        waits: holds
            .iter()
            .map(|h| HoldWait {
                reason: h.reason,
                held_ms: h.held_ms,
            })
            .collect(),
    })
}

/// Why a pin moved, in the order the rules run: the first rule with a destination moves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrateReason {
    /// A cold pin left an account winding down toward its end.
    Drain,
    /// A subscription account in overage handed its session back to plan quota.
    Overage,
    /// A pin on an api_key account returned to plan quota as `return_to_plan` says: cold, or warm past the return dwell
    /// under `always`.
    ReturnToPlan,
    /// A shedding round marked the pin.
    Shed,
    /// The pinned account's window for the request's model is spent while a sibling holds allowance in it.
    Spent,
}

impl MigrateReason {
    /// The order step 4 runs its rules in, which the walk keeps: a fired rule's successors read `not_run`.
    const ORDER: [MigrateReason; 5] = [
        MigrateReason::Spent,
        MigrateReason::Overage,
        MigrateReason::ReturnToPlan,
        MigrateReason::Drain,
        MigrateReason::Shed,
    ];

    /// The rule's walk step.
    fn step(self) -> &'static str {
        match self {
            MigrateReason::Drain => "move:drain",
            MigrateReason::Overage => "move:overage",
            MigrateReason::ReturnToPlan => "move:return_to_plan",
            MigrateReason::Shed => "move:shed",
            MigrateReason::Spent => "move:spent",
        }
    }

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

/// Which classes refresh a pin's warmth: `main` and `subagent` re-arm a recipe, and a completed `compaction` stops it
/// until the next main turn. Every other class is auxiliary.
pub const REFRESHING_CLASSES: &[&str] = &["compaction", "main", "subagent"];

/// The agent id of a session's main conversation, the one a request without an agent header belongs to.
pub const MAIN_AGENT: &str = "main";

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
/// restarted it, and one matching neither is foreign. A body without a chain refreshes, the fallback for a request
/// that carries no fact. A keepalive probe replays the recipe's headers and body verbatim, the re-arming request's
/// rather than its own, so it is judged by none of this: its renewal report stamps the refresh instead. A token count
/// is judged by none of it either, since it creates no cache entry to be warm about. The cause beside the verdict
/// names the branch that decided it.
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
    if request.static_hash().is_none() && request.boundaries.is_empty() {
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
    // The static prefix without a continuation restarted the conversation.
    if request
        .static_hash()
        .is_some_and(|hash| Some(hash) == current.static_hash.as_ref())
    {
        return (Warmth::Cold, WarmthCause::StaticRestart);
    }
    (Warmth::Untouched, WarmthCause::Foreign)
}

fn warm_on(current: &PinRow, verdict: Warmth, now: i64) -> bool {
    match verdict {
        Warmth::Cold => false,
        Warmth::Refresh => true,
        Warmth::Untouched => warm(current, now),
    }
}

/// The flag a placement gives its pin: set for a request of a class outside `REFRESHING_CLASSES`, whose kind keeps the
/// prefix warm without refreshing it, and clear for a refreshing class or a request that names none.
fn placed_auxiliary_only(request: &SelectRequest) -> bool {
    request
        .request_class
        .as_deref()
        .is_some_and(|class| !REFRESHING_CLASSES.contains(&class))
}

/// The request's two hashes, for a pin it refreshed, colded or created: the static prefix and the last message
/// boundary, which the next continuation's boundaries contain.
fn stamp_hashes(pin: &mut PinRow, request: &SelectRequest) {
    pin.static_hash = request.static_hash().cloned();
    pin.tail_hash = request.boundaries.last().cloned();
}

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
    /// `subscription_ended`), or a partition's name: `at_cap`, `no_fit`, `spent`, `winding_down`, or `ranked` for an
    /// eligible sibling the strategy did not pick.
    pub obstacle: &'static str,
}

/// Whether an account's window set holds each of the two shared plan windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedWindows {
    pub five_hour: bool,
    pub seven_day: bool,
}

/// The window set of `account_id` as a selection that lands on it names it.
fn shared_windows(state: &RoutingState, account_id: &str) -> SharedWindows {
    SharedWindows {
        five_hour: state.has_window(account_id, FIVE_HOUR),
        seven_day: state.has_window(account_id, SEVEN_DAY),
    }
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
    /// The shared windows the account's window set holds at this selection, so the relay reads a window a head leaves
    /// out against the windows the account has. Both for an account with no sample and for a set no clean poll has
    /// settled.
    pub shared_windows: SharedWindows,
    pub trace: SelectionTrace,
    /// The pool the candidates were resolved under: a selection re-resolved on a newer revision may name another
    /// pool than the request was admitted under, which the relay refuses rather than dispatch under one pool's
    /// policy on another's member.
    pub pool_id: String,
    /// The account the session's previous request landed on, where this selection lands elsewhere: the pin row
    /// step 1 read, live, idle past the TTL or left behind by its pool, the newest across the session's models,
    /// leaving out a pin only auxiliary calls used, unless the request is an auxiliary call, else the landing a
    /// failover carried to this request where it takes a pin. A renewal names it too, after a turn of the session on
    /// another account. `None` on a count's bound and a placement the routing holds no previous landing for. The relay
    /// names no previous message on a request that carries it, since the account cannot find one.
    pub left: Option<String>,
    /// `Some` for the client's classifier call alone: its charge and the pin of the turn it judges.
    pub judged: Option<Judged>,
    /// The `last_input_tokens` of the pin row step 1 read, where the selection lands on that row's account and the
    /// turn is none of `compacted`, `lane_changed` and `static_restart`. `None` in every other case: no pin row at
    /// step 1, a row that has seen no usage report, a landing on another account and the three turns.
    pub previous_input_tokens: Option<u64>,
    /// Whether the cache entry of the pin row step 1 read was past its lifetime at the request, before the request's
    /// own refresh. `None` wherever `previous_input_tokens` is `None`.
    pub entry_expired: Option<bool>,
    /// Whether that row carried `ended` when the routing task took the request, which `SelectRequest::pin_ended` tells.
    /// `None` wherever `previous_input_tokens` is `None`.
    pub agent_released: Option<bool>,
}

/// A classifier call's standing against the turn it judges: the upstream keeps the call free on the account and
/// incarnation that turn's stream ended on, and charges it as any call elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgedCharge {
    On,
    Off,
}

/// What a selection read for the client's classifier call about the turn it judges, the one value the relay hands
/// back on the call's usage report.
#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    /// `On` where the selected account and incarnation are the ones the judged turn's stream ended on, `Off` elsewhere
    /// and in a session with no turn to judge.
    pub charge: JudgedCharge,
    /// The pin of the judged turn, as `decide` read it ahead of the run, `None` in a session with no turn to judge. The
    /// call's tokens fold into it, never into the pin of a turn that ended while the call was upstream.
    pub pin: Option<PinRef>,
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

/// The reason an upstream rate limit cools an account or a model under, the one reason a usage reading may lift
/// early.
pub const RATE_LIMIT_REASON: &str = "rate_limit";

/// The window of a cooldown only a `Retry-After` timed. It names no plan window, so no reading lifts it early, and
/// the relay may wait it out in place.
pub const RETRY_AFTER_WINDOW: &str = "retry_after";

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
    /// A probe that finds no live pin, or whose pinned member cannot serve it, answered on that standing and placed
    /// nowhere, so a probe never moves the session's pin and creates none. A wait answers 429 and every other standing
    /// 503. The obstacle reaches the keepalive, which holds the agent under it.
    Pinned {
        obstacle: PinnedObstacle,
    },
}

/// The standing a probe's pinned answer names. A probe warms the pin it was sent for, and a placement on a sibling
/// would move the session's pin onto a member no request asked for, so the selector answers the standing instead.
/// No wait clears `attempted` (a member this probe already tried while a sibling could take it), `budget_unknown` (a
/// budgeted member whose spend the degraded ledger leaves unknown), `operator` (an account the operator paused, or one
/// deleted or in a state other than ready that the probe did not try while a sibling could take it),
/// `subscription_ended` (one past its saved end) or `no_pin`, so each answers untimed. `cooldown`, `credits`, `overage`
/// and `spent` name a wait while a sibling could take the probe, dated where the obstacle names its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinnedObstacle {
    Attempted,
    BudgetUnknown,
    /// The pinned member cools until `until` while a sibling could take the probe.
    Cooldown {
        until: i64,
    },
    /// The pinned member's usage credits are out for a fast probe.
    Credits,
    /// The probe's session holds no live pin, so it has no entry to renew.
    NoPin,
    Operator,
    /// The pinned member is in overage until `until` under a pool that refuses it.
    Overage {
        until: i64,
    },
    /// The pinned member is a usage-based seat at its spend cap until `until`.
    Spent {
        until: i64,
    },
    SubscriptionEnded,
}

impl PinnedObstacle {
    /// The word the exhausted decision's `obstacle` fact records: the one `Obstacle::name` gives the selector's own
    /// obstacle, or `no_pin`.
    pub fn as_str(self) -> &'static str {
        match self {
            PinnedObstacle::Attempted => "attempted",
            PinnedObstacle::BudgetUnknown => "budget_unknown",
            PinnedObstacle::Cooldown { .. } => "cooldown",
            PinnedObstacle::Credits => "credits",
            PinnedObstacle::NoPin => "no_pin",
            PinnedObstacle::Operator => "operator",
            PinnedObstacle::Overage { .. } => "overage",
            PinnedObstacle::Spent { .. } => "spent",
            PinnedObstacle::SubscriptionEnded => "subscription_ended",
        }
    }

    /// The instant a wait clears the obstacle at, for the three that name one.
    pub const fn until(self) -> Option<i64> {
        match self {
            PinnedObstacle::Cooldown { until }
            | PinnedObstacle::Overage { until }
            | PinnedObstacle::Spent { until } => Some(until),
            PinnedObstacle::Attempted
            | PinnedObstacle::BudgetUnknown
            | PinnedObstacle::Credits
            | PinnedObstacle::NoPin
            | PinnedObstacle::Operator
            | PinnedObstacle::SubscriptionEnded => None,
        }
    }

    /// Whether a wait clears the obstacle: the three dated ones and `Credits`.
    pub const fn waits(self) -> bool {
        match self {
            PinnedObstacle::Cooldown { .. }
            | PinnedObstacle::Credits
            | PinnedObstacle::Overage { .. }
            | PinnedObstacle::Spent { .. } => true,
            PinnedObstacle::Attempted
            | PinnedObstacle::BudgetUnknown
            | PinnedObstacle::NoPin
            | PinnedObstacle::Operator
            | PinnedObstacle::SubscriptionEnded => false,
        }
    }
}

/// The HTTP status the relay answers an exhaustion with, which the exhausted decision's `answer` fact records too.
/// `at_cap` is a request held at the cap, its wait run out or an account passed over, with no leg dialed: its untimed
/// exhaustion is a pool at its in-flight cap, which frees on its own (429), rather than one only an operator can
/// clear (503). A `const fn`, so the relay's constant failures take their status from it.
pub const fn exhaustion_status(exhaustion: &Exhaustion, at_cap: bool) -> u16 {
    match exhaustion {
        Exhaustion::Untimed if at_cap => 429,
        Exhaustion::ModelRejected => 404,
        Exhaustion::Pinned { obstacle } if obstacle.waits() => 429,
        Exhaustion::Untimed | Exhaustion::Pinned { .. } => 503,
        Exhaustion::Timed { .. } | Exhaustion::FastLane { .. } | Exhaustion::FastCredits { .. } => {
            429
        }
    }
}

/// Why the current registry refuses a request admitted under an older one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The registry no longer holds the key the request was admitted on, or holds another key under its id.
    Key(KeyRefusal),
    Resolve(ResolveError),
}

impl Refusal {
    /// The structured word a refused decision records beside the display reason.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Key(refusal) => refusal.code(),
            Refusal::Resolve(refusal) => refusal.code(),
        }
    }

    /// The refusal as its record carries it: the router's own kind `key`, the code and the display reason.
    pub(crate) fn facts(&self) -> RefusalFacts {
        RefusalFacts {
            kind: "key",
            code: self.code(),
            reason: match self {
                Refusal::Key(refusal) => refusal.to_string(),
                Refusal::Resolve(refusal) => refusal.to_string(),
            },
        }
    }
}

// Boxing the selection would put an allocation on the path every request takes, to shrink a value that is
// built once per selection and moved once into the answering channel.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum SelectResult {
    Exhausted(Exhaustion),
    /// The request waits before its head, the hold's `reason` saying why: every account the selection could place on
    /// holds its cap (the hold's `account_id` the one it would have chosen), the pinned account's spent window resets
    /// inside the retry bound, or the pin's account is slowed and the pin waits its deadline.
    Held(HoldFacts),
    Refused(Refusal),
    Selected(Selection),
}

/// Why a candidate is out of the eligible set. Only a budget, a cooldown, an overage and a spent window carry a
/// deadline the caller can wait for.
#[derive(Clone, Debug, PartialEq)]
pub enum Obstacle {
    Attempted,
    /// The member's spend reached one of its budgets. `until` is the latest end among the budgets reached and `period`
    /// the budget that end belongs to, the one the record names.
    Budget {
        until: i64,
        period: BudgetPeriod,
    },
    /// The ledger is degraded, so the member's spend rows may be stale. A budgeted member is not placed on until the
    /// writer catches up and the spend is read afresh. Untimed: no deadline clears it.
    BudgetUnknown,
    Cooldown {
        reason: String,
        until: i64,
        window: String,
        scope: CooldownScope,
    },
    /// The poll reads the account's usage credits as unavailable for fast mode.
    Credits {
        reason: String,
    },
    /// The registry no longer holds the account.
    Deleted,
    /// Canceled and past the saved end.
    Ended,
    /// The account is in overage until `until`, which the pool's `never` policy refuses.
    Overage {
        until: i64,
    },
    /// The operator paused the account.
    Paused,
    /// The account has no allowance left in `window` until `until`: the seat's spend cap under `spend`, or the
    /// model's own week where the model-window partition passes the account over.
    Spent {
        until: i64,
        window: String,
    },
    /// The account's state is not `ready`. The selection reads it through `unready`, which admits a
    /// `funding_required` account whose retry is due, and a feature call through `feature_ready`, which admits a
    /// `ready` account alone.
    Unready,
}

impl Obstacle {
    /// The step-2 name `release_reasons` and the trace report for an obstacle: `attempted`, `budget`,
    /// `budget_unknown`, `cooldown`, `credits`, `operator`, `overage`, `spent`, or `subscription_ended`.
    pub fn name(&self) -> &'static str {
        match self {
            Obstacle::Attempted => "attempted",
            Obstacle::Budget { .. } => "budget",
            Obstacle::BudgetUnknown => "budget_unknown",
            Obstacle::Cooldown { .. } => "cooldown",
            Obstacle::Credits { .. } => "credits",
            Obstacle::Deleted | Obstacle::Paused | Obstacle::Unready => "operator",
            Obstacle::Ended => "subscription_ended",
            Obstacle::Overage { .. } => "overage",
            Obstacle::Spent { .. } => "spent",
        }
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

/// Each budget the member carries beside the ledger's spend row for its period, empty for a member without one.
pub(crate) fn budget_facts(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool_id: &str,
    account_id: &str,
) -> Vec<BudgetFacts> {
    budgeted_member(snapshot, pool_id, account_id)
        .map(|member| {
            let spends: Vec<&MemberSpend> = state.member_spends_of(pool_id, account_id).collect();
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
        .unwrap_or_default()
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

/// The prompt size a pin's burn is divided by and its move rewrites: the pin's own prompt, else the request's own body
/// where no call has renewed it with usage. A move to `destination` over a pair whose thinking blocks are bound drops
/// the blocks the pin's account produced and keeps the destination's own, so its size is the last prompt less the
/// pin's sum for its account plus its sum for the destination, never under zero.
fn prompt_size(
    state: &RoutingState,
    pin: &PinRow,
    request: &SelectRequest,
    destination: Option<&str>,
) -> u64 {
    if pin.last_input_tokens == 0 {
        return request.body_tokens;
    }
    match bound_destination(state, pin, destination) {
        Some(destination) => {
            let sum = |account: &str| pin.thinking_sums.get(account).map_or(0, |sum| sum.tokens);
            pin.last_input_tokens
                .saturating_add(sum(destination))
                .saturating_sub(sum(&pin.account_id))
        }
        None => pin.last_input_tokens,
    }
}

/// `destination` where the pin's move there drops the blocks its account produced: another account whose pair with
/// the pin's reads bound for the pin's model.
fn bound_destination<'a>(
    state: &RoutingState,
    pin: &PinRow,
    destination: Option<&'a str>,
) -> Option<&'a str> {
    destination
        .filter(|destination| *destination != pin.account_id)
        .filter(|destination| state.bound(&pin.model, &pin.account_id, destination))
}

/// Whether the selection sets the session's thinking sums to zero: a compacted request's prompt holds no earlier
/// block. A probe replays the header of the request it keeps warm, whose answer has added its thinking since.
pub(crate) fn zeroes_sums(request: &SelectRequest) -> bool {
    request.context_compacted && !request.probe
}

/// The thinking sums a selection writes on the session's row from the row it stood on: each entry whose holder
/// `snapshot` still carries, at zero where `zeroes_sums` reads the request. A failover's row and a deleted account's
/// trace wait outside the state, where the purge's pass does not walk, so the hand-over drops a gone holder's sum
/// itself.
pub(crate) fn handed_sums(
    row: &PinRow,
    request: &SelectRequest,
    snapshot: &RegistrySnapshot,
) -> BTreeMap<String, ThinkingSum> {
    let zeroed = zeroes_sums(request);
    row.thinking_sums
        .iter()
        .filter(|(account, sum)| snapshot.holder(account, sum.incarnation).is_some())
        .map(|(account, sum)| {
            let tokens = if zeroed { 0 } else { sum.tokens };
            (account.clone(), ThinkingSum { tokens, ..*sum })
        })
        .collect()
}

/// The rewrite a selection prices for the pin's next write: its last context, or the body's estimate for a fresh
/// session or a pin never renewed with usage, as one cache write at the lane's TTL (five minutes until a pin reports
/// one), sized for `destination` where the caller knows it. The gate's booking and the pick's figure price the same
/// tokens through it.
pub(crate) fn rewrite_write(
    state: &RoutingState,
    request: &SelectRequest,
    pin: Option<&PinRow>,
    destination: Option<&str>,
) -> PinUsage {
    // The plan windows see nothing of the client's classifier call, so its body is no rewrite to fit or to book. A
    // renewed pin's own context stays the session's, since a move still rewrites it.
    let body = if request.classifier {
        0
    } else {
        request.body_tokens
    };
    // A pin never renewed with usage reads its context from the body, as a fresh session does, and a fresh
    // session's TTL is unknown before a pin exists, so its write prices at five minutes.
    let (tokens, ttl_ms) = match pin {
        Some(pin) => (
            prompt_size(state, pin, request, destination),
            pin.cache_ttl_ms,
        ),
        None => (body, FIVE_MINUTES_MS),
    };
    cache_write(tokens, ttl_ms)
}

fn cache_write(tokens: u64, ttl_ms: i64) -> PinUsage {
    if ttl_ms >= ONE_HOUR_MS {
        PinUsage {
            write_1h: tokens,
            ..PinUsage::default()
        }
    } else {
        PinUsage {
            write_5m: tokens,
            ..PinUsage::default()
        }
    }
}

/// The rewrite per plan window (`rewrite_write`'s write on `tier` at the band of `now`) and the burn per plan window
/// (the pin's own decayed burn over its account's speed, or for a fresh session the decayed burn of the row a failover
/// took off its source over that source's speed, else the median of the pool's priced warm pins, each over its
/// account's speed, absent with none, which reads as zero), on the windows the request's model draws that the
/// destination `account_id` has. A window nothing prices is absent from both. The destination gate's fit and the
/// shedding round read it. The third member is the entrypoint the burn is priced under (the pin's, the failed-over
/// row's, or the request's for the median), whose band a reader that weighs the burn over a span reads it under.
pub(crate) fn pin_figures<'a>(
    state: &RoutingState,
    pool: &PoolRow,
    account_id: &str,
    request: &'a SelectRequest,
    figures: Figures<'a>,
    tier: &str,
    now: i64,
) -> (
    BTreeMap<String, f64>,
    BTreeMap<String, f64>,
    Option<&'a str>,
) {
    let share = state.share();
    let model = request.canonical_model.as_str();
    let average_ms = burn_average_ms(pool);
    let write = rewrite_write(state, request, figures.pin(), Some(account_id));
    let priced = |window: &str, entrypoint: Option<&str>, p: &PinRow| {
        share
            .rate(window, entrypoint, tier, &p.model, &p.burn, now)
            .map(|rate| burn_decayed(rate, p.burn_at, now, average_ms))
    };
    let regime = match figures {
        Figures::Pin(pin) => pin.entrypoint.as_deref(),
        Figures::Fresh(_) => match &request.failed_over {
            Some(failed_over) => failed_over.row.entrypoint.as_deref(),
            None => request.entrypoint.as_deref(),
        },
    };
    let mut rewrite = BTreeMap::new();
    let mut burn = BTreeMap::new();
    for window in state.plan_windows_held(account_id, model) {
        // Nothing priced, or a share of nothing, is nothing to book.
        if let Some(share) = share
            .price(
                window,
                request.entrypoint.as_deref(),
                tier,
                model,
                write,
                now,
            )
            .filter(|s| *s > 0.0)
        {
            rewrite.insert(window.to_string(), share);
        }
        // A slowed pin's burn is what its source's holds let through, so a destination reads the demand: the burn over
        // the source's speed, which the pin runs at there until the destination's own factor says otherwise.
        let rate = match figures {
            Figures::Pin(pin) => priced(window, regime, pin)
                .map(|rate| rate / state.slow_down_speed(pool, &pin.account_id, now)),
            // A failover's landing knows the session's own burn, while the destination's cache is cold, so the
            // rewrite above stays the body's.
            Figures::Fresh(_) if request.failed_over.is_some() => {
                request.failed_over.as_ref().and_then(|failed_over| {
                    priced(window, regime, &failed_over.row).map(|rate| {
                        rate / state.slow_down_speed(pool, &failed_over.from_account_id, now)
                    })
                })
            }
            // A pin burns the windows its own model draws, so a Sonnet pin adds nothing to a Fable week's prior. The
            // unpriced ones are left out of the median so one of them does not zero it. A fresh session starts
            // unslowed, so each warm pin counts at its unslowed rate, over its own account's speed. The meter charges
            // the new session under the request's regime, which its pin carries and its booking names, so every warm
            // pin's tokens price under that regime and not their own.
            Figures::Fresh(warm_pins) => median(
                warm_pins
                    .iter()
                    .filter(|p| plan_windows(&p.model).any(|w| w == window))
                    .filter_map(|p| {
                        priced(window, regime, p)
                            .map(|rate| rate / state.slow_down_speed(pool, &p.account_id, now))
                    })
                    .collect(),
            ),
        };
        if let Some(rate) = rate.filter(|r| *r > 0.0) {
            burn.insert(window.to_string(), rate);
        }
    }
    (rewrite, burn, regime)
}

/// Whose rewrite and burn `pin_figures` prices.
#[derive(Clone, Copy)]
pub(crate) enum Figures<'a> {
    /// A live pin's own: its last context and its decayed burn.
    Pin(&'a PinRow),
    /// A fresh session's: the body and the median burn of the pool's warm pins, which `fresh_prior` collects once per
    /// walk since every candidate of the walk reads the same set. A failed-over request's landing reads the burn of the
    /// row its source lost here instead of the median.
    Fresh(&'a [PinRow]),
}

impl<'a> Figures<'a> {
    /// `pin`'s figures, or the fresh session's from `prior` where there is no pin.
    fn of(pin: Option<&'a PinRow>, prior: &'a [PinRow]) -> Self {
        pin.map_or(Figures::Fresh(prior), Figures::Pin)
    }

    fn pin(self) -> Option<&'a PinRow> {
        match self {
            Figures::Pin(pin) => Some(pin),
            Figures::Fresh(_) => None,
        }
    }
}

/// A fresh session's prior: the warm pins the pool keeps on its members at `now`, each priced per window by
/// `pin_figures`. The key-to-pool filter of `in_pool` leaves out a pin a former pool left on a member, as the
/// projections leave it out of the standing burn, so another pool's sessions shape no placement of this one. The rows
/// come in no order, since `median` sorts the rates it reads.
pub(crate) fn fresh_prior(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    now: i64,
) -> Vec<PinRow> {
    let members: HashSet<&str> = snapshot
        .members(&pool.id)
        .iter()
        .map(|m| m.account_id.as_str())
        .collect();
    let kept = in_pool(snapshot, pool);
    state
        .pin_rows()
        .filter(|p| members.contains(p.account_id.as_str()) && kept(p) && warm(p, now))
        .cloned()
        .collect()
}

/// The median of `values`, the mean of the two middle ones for an even count, `None` for none.
fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

/// Whether an account can still serve a request whose allowance is judged in `window` (`window_for`'s answer):
/// for a model with a week of its own, that week and both shared plan windows, since its requests draw on the
/// shared windows too; both shared plan windows otherwise. A prediction at 1.0 is spent. No sample, or a sample
/// that never read the window, counts as allowance (an api_key account, whose sample carries no plan window of
/// its own, always holds it here). A reading past its reset, or one a newer head lowered, no longer excludes. A
/// usage-based account at its spend cap (`spend_spent`) has none whatever the request's window, since the spend funds
/// every model, and below it holds allowance whatever its plan-window readings say: the spend is its one hold, and a
/// reading it kept from before the class, or a head's family it still records, stops no request.
fn holds_allowance(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &str,
    now: i64,
) -> bool {
    if spend_spent(state, snapshot, account_id, now).is_some() {
        return false;
    }
    if snapshot.usage_based(account_id) {
        return true;
    }
    allowance_windows(window).all(|name| spent_reading(state, account_id, name, now).is_none())
}

/// The account's prediction on the plan window `window` where it stands at 1.0 or past it, so the window has no
/// allowance left.
fn spent_reading(
    state: &RoutingState,
    account_id: &str,
    window: &str,
    now: i64,
) -> Option<WindowReading> {
    state
        .predicted(account_id, window, now)
        .filter(|r| r.utilization >= 1.0)
}

/// The plan windows `holds_allowance` reads for a request judged in `window`: the model's own week where `window` is
/// one, then both shared windows.
fn allowance_windows(window: &str) -> impl Iterator<Item = &str> {
    (window != SEVEN_DAY)
        .then_some(window)
        .into_iter()
        .chain([FIVE_HOUR, SEVEN_DAY])
}

/// The spent windows `holds_allowance` reads for `window`, each with its reset, `None` for one that names none, and
/// the window `spend` at its reset where `spend_spent` holds the account out. A usage-based account lists no plan
/// window, as `holds_allowance` reads none for it.
fn spent_windows<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &'a str,
    now: i64,
) -> Vec<(Option<i64>, &'a str)> {
    let usage_based = snapshot.usage_based(account_id);
    allowance_windows(window)
        .filter(|_| !usage_based)
        .filter_map(|name| spent_reading(state, account_id, name, now).map(|r| (r.reset, name)))
        .chain(spend_spent(state, snapshot, account_id, now).map(|reset| (Some(reset), "spend")))
        .collect()
}

/// The spend window's reset where a usage-based account's share reads at or above one at `now`: the one read the
/// candidate's obstacle, the allowance and the spent windows share, so the hold and its deadline never disagree.
fn spend_spent(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    now: i64,
) -> Option<i64> {
    let window = state.spend_window(snapshot, account_id, now)?;
    window
        .utilization
        .is_some_and(|share| share >= 1.0)
        .then_some(window.reset)
}

/// The latest reset among the spent windows that name one, with that window's key: the deadline an exhaustion
/// answers for an account step 6 passes over as spent. A spent window that names no reset adds nothing, so an account
/// spent on a dated and an undated window still dates the answer. `None` when no spent window names a reset.
fn spent_until<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &'a str,
    now: i64,
) -> Option<(i64, &'a str)> {
    spent_windows(state, snapshot, account_id, window, now)
        .into_iter()
        .filter_map(|(reset, name)| reset.map(|reset| (reset, name)))
        .max_by_key(|(reset, _)| *reset)
}

/// The moment the account can serve the request again once every spent window has reset, with the key of the window
/// that resets last. `None` when no window is spent, and when a spent window names no reset, since a wait would end
/// with that window still spent.
fn refunded_at<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &'a str,
    now: i64,
) -> Option<(i64, &'a str)> {
    if spent_windows(state, snapshot, account_id, window, now)
        .iter()
        .any(|(reset, _)| reset.is_none())
    {
        return None;
    }
    spent_until(state, snapshot, account_id, window, now)
}

/// The pool's affinity in milliseconds, saturating for a value no clock reaches.
pub(crate) fn affinity_ttl_ms(pool: &PoolRow) -> i64 {
    seconds_ms(pool.affinity_ttl_seconds)
}

/// The state that keeps `account` out of the set, `None` for a ready account. A funding mark is tried again on its own
/// clock, one request at a time, since an API-key balance has no signal the poll could read, so a mark whose retry
/// the selection admits stands against nothing.
fn unready(state: &RoutingState, account: &AccountRow, now: i64) -> Option<&'static str> {
    let retry_admitted = account.state == AccountState::FundingRequired
        && state.funding_retry_admits(&account.id, now);
    not_ready(account).filter(|_| !retry_admitted)
}

/// The stored state that keeps `account` from serving, by its word, `None` for a ready account.
fn not_ready(account: &AccountRow) -> Option<&'static str> {
    (account.state != AccountState::Ready).then(|| account.state.as_str())
}

/// The facts a selection finds against a pinned account, in the order it evaluates them, for two events: the
/// `pin_expired` of a pin released without a migration, and the `pin_migrated` of a live pin a placement replaced on
/// another account, which carries them whole and takes its word from them. They are `expired` (idle past the pool's
/// affinity TTL), `unserved` (the account no longer serves the model for this key: it left the pool, the model was
/// deselected, or the key or the pool is gone, and the account is still in the registry), `deleted` (the account left
/// the registry), `paused` (`enabled` is off), the account's state when it cannot serve (`disconnected`,
/// `funding_required`, `reauth_required`, `unpaired`), `subscription_ended` (canceled and past its saved end),
/// `cooldown` (a live row blocks the model on the account), `fast_rate_limit` or `fast_credits` for a fast request (one
/// of the fast lane's two rows, or the poll's credits state, blocks it), `budget_unknown` (the member carries a budget
/// while the ledger is degraded) or else `budget` (the member's spend is at or above its configured budget), `overage`
/// (in overage under a policy other than `allow`), `attempted` (this request already tried it), and `spent` (no
/// allowance left in the request's window). A fast request draws no plan window, so neither `overage` nor `spent`
/// stands against it. Never empty: a pin released by a move whose destination then answered nothing carries
/// `released`, the one fact the list above does not evaluate.
fn release_reasons(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pin: &PinRow,
    request: &SelectRequest,
) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    let pool = snapshot.key_pool(&request.key_id);
    if pool.is_some_and(|pool| request.now - pin.last_used_at > affinity_ttl_ms(pool)) {
        reasons.push("expired");
    }
    let serves = request
        .candidates
        .iter()
        .any(|c| c.account_id == pin.account_id);
    match snapshot.account(&pin.account_id) {
        None => reasons.push("deleted"),
        Some(account) => {
            if pool.is_none() || !serves {
                reasons.push("unserved");
            }
            if !account.enabled {
                reasons.push("paused");
            }
            if let Some(name) = unready(state, account, request.now) {
                reasons.push(name);
            }
        }
    }
    if subscription_ended(snapshot, &pin.account_id, request.now) {
        reasons.push("subscription_ended");
    }
    if let Some(row) = state.blocking_cooldown(
        &pin.account_id,
        &request.canonical_model,
        request.now,
        request.fast,
    ) {
        // A fast-lane row names its own reason, so the drawer never reads it as a cooled account.
        reasons.push(match row.scope {
            CooldownScope::Fast => FAST_RATE_LIMIT_REASON,
            CooldownScope::FastCredits => FAST_CREDITS_REASON,
            CooldownScope::Account | CooldownScope::Model(_) => "cooldown",
        });
    }
    // The credits row blocks beside a later row too, and so does the poll's state, which no row records.
    if request.fast
        && !reasons.contains(&FAST_CREDITS_REASON)
        && (fast_credits_row(state, &pin.account_id, request.now).is_some()
            || state
                .credits_blocked(snapshot, &pin.account_id, request.now)
                .is_some())
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
            snapshot,
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

/// The order that picks the one word a replaced pin's move carries out of what stood against its account: what the
/// account is first, then what its pool and its limits say, so a deleted account's pin reads `deleted` whatever its
/// list holds behind it. The reload's purge takes a deleted account's pin before any selection reads it, and its
/// landing takes `deleted` from the purge's entry as a failover's takes its word. The word stands here so the order
/// covers every word `release_reasons` answers but `expired`, which the selection never reads as a move.
const MOVE_CAUSE_ORDER: [&str; 17] = [
    "deleted",
    "paused",
    AccountState::Disconnected.as_str(),
    AccountState::FundingRequired.as_str(),
    AccountState::ReauthRequired.as_str(),
    AccountState::Unpaired.as_str(),
    "subscription_ended",
    "unserved",
    "cooldown",
    FAST_RATE_LIMIT_REASON,
    FAST_CREDITS_REASON,
    "budget_unknown",
    "budget",
    "overage",
    "attempted",
    "spent",
    "released",
];

/// The word a live pin's move onto another account carries: the first of `MOVE_CAUSE_ORDER` that `reasons`, the
/// pin's `release_reasons`, holds, else `released`.
pub(crate) fn move_word(reasons: &[&'static str]) -> &'static str {
    MOVE_CAUSE_ORDER
        .into_iter()
        .find(|word| reasons.contains(word))
        .unwrap_or("released")
}

/// One decision, whole: the selection's answer and its writes, with the record and the row summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Decided {
    pub result: SelectResult,
    pub writes: Vec<StateWrite>,
    pub record: DecisionRecord,
    pub summary: DecisionSummary,
    /// The round this decision ran, for the task's `SheddingRound` event, or `None` when none ran.
    pub shed_round: Option<ShedRoundRecord>,
}

/// A shedding round a decision ran: the pin it ran on and that pin's account, which the round's event names, beside
/// what the round read.
#[derive(Clone, Debug, PartialEq)]
pub struct ShedRoundRecord {
    pub account_id: String,
    pub pin: PinKey,
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
    // Sized once for the longest walk, so a decision appends its steps without growing the vector.
    let mut rec = Recorder {
        walk: Vec::with_capacity(WALK_STEPS.len()),
        // Read against the state the request found, ahead of the run's writes, which move or replace the pin.
        released: state
            .pin(&request.pin_key())
            .map(|pin| release_reasons(state, snapshot, pin, request))
            .unwrap_or_default(),
        ..Recorder::default()
    };
    let key = snapshot.key(&request.key_id);
    let pool = key.and_then(|k| snapshot.pool(&k.pool_id));
    // Read ahead of the run, whose writes stamp and move the session's pins: the call judges the turn that stood when
    // it arrived, the one `judged_candidate` directs it by, and its usage folds into that turn's pin whatever turn ends
    // while it is upstream.
    let judged = request.classifier.then(|| JudgedTurn {
        end: state
            .latest_turn_end(&request.key_id, &request.session_id)
            .map(|end| (end.account_id.clone(), end.incarnation)),
        pin: state
            .latest_turn_pin(&request.key_id, &request.session_id)
            .map(|pin| (pin_ref(pin), pin.account_id.clone(), pin.incarnation)),
    });
    let (mut result, writes) = run(
        state,
        snapshot,
        pool,
        request,
        judged.as_ref(),
        rng,
        &mut rec,
    );
    if let Some(JudgedTurn { end, pin }) = judged {
        let (judged_pin, pin_turn) = pin
            .map(|(pin, account_id, incarnation)| (pin, (account_id, incarnation)))
            .unzip();
        // The account and incarnation the judged turn ran on: the recorded stream end's, which a move of the turn's pin
        // since leaves as it was, else the pin's as it stands, the reading after a restart. `None` in a session with no
        // turn to judge.
        let turn = end.or(pin_turn);
        rec.judged_pin_step(
            turn.as_ref().map(|(account_id, _)| account_id.as_str()),
            judged_pin.as_ref(),
        );
        if let SelectResult::Selected(selection) = &mut result {
            let on = turn.is_some_and(|(account_id, incarnation)| {
                account_id == selection.account.id && incarnation == selection.account.incarnation
            });
            selection.judged = Some(Judged {
                charge: if on {
                    JudgedCharge::On
                } else {
                    JudgedCharge::Off
                },
                pin: judged_pin,
            });
        }
    }
    // The reserve station draws on every walk. A placement and a move record it where they book, and an answer that
    // never reached the booking states here that it booked nothing, naming the account it landed or waits on.
    if rec.walk.iter().all(|step| step.step != "reserve") {
        let account = match &result {
            SelectResult::Selected(selection) => Some(selection.account.id.as_str()),
            SelectResult::Held(hold) => Some(hold.account_id.as_str()),
            SelectResult::Exhausted(_) | SelectResult::Refused(_) => None,
        };
        rec.step(
            "reserve",
            "none",
            account.map(account_fact).into_iter().collect(),
        );
    }
    let (record, summary, shed_round) = finish(
        rec,
        request,
        key.map_or("", |k| k.pool_id.as_str()),
        pool,
        &result,
    );
    Decided {
        result,
        writes,
        record,
        summary,
        shed_round,
    }
}

/// The round `move_of` just ran, as the registry keeps it: the round's stamp, then each marked pin's row with its
/// mark, read back from the state the round marked them on.
fn round_write(
    state: &RoutingState,
    pool: &PoolRow,
    current: &PinRow,
    at: i64,
    round: &ShedRoundRecord,
) -> Vec<StateWrite> {
    let mut writes = vec![StateWrite::ShedRound {
        pool_id: pool.id.clone(),
        account_id: round.account_id.clone(),
        incarnation: current.incarnation,
        round_at: at,
    }];
    for mark in &round.facts.marks {
        let key = PinKey::new(&mark.key_id, &mark.session_id, &mark.agent_id, &mark.model);
        if let Some(row) = state.pin(&key) {
            writes.push(StateWrite::UpsertPin(row.clone()));
        }
    }
    writes
}

/// What a decision read while it ran, appended where each fact was read, so the record holds the engine's own
/// readings and never a second look at the state.
#[derive(Default)]
struct Recorder {
    walk: Vec<WalkStep>,
    candidates: Vec<CandidateFacts>,
    pin: Option<PinFacts>,
    pin_reason: Option<&'static str>,
    /// What stood against the account of the pin the request found, as `release_reasons` read it before the run moved
    /// anything, for the pin block. Empty where the request found no pin.
    released: Vec<&'static str>,
    rule: Option<MigrateReason>,
    source: Option<String>,
    /// A move no rule made, a failover's landing or a live pin replaced on another account: its word and the account
    /// it left, which the outcome names as its rule and its source. The members' verdicts read `source` alone, so the
    /// account keeps the verdict its own standing gave it.
    moved: Option<(&'static str, String)>,
    destinations: Vec<String>,
    pin_written: Option<PinWritten>,
    hold_place: Option<&'static str>,
    shed_round: Option<ShedRoundRecord>,
    /// The plan windows a round on the renewed pin's account tests, in the trigger's order, for the renewal fact.
    trigger_windows: Vec<&'static str>,
    /// Each projected window's round limit by window, index `i` for `candidates[i]`, beside the projection the
    /// member's facts carry, so the renewal fact tests that projection against its round's limit.
    horizon_bounds: Vec<BTreeMap<&'static str, f64>>,
    /// The mark step 1's heal check or step 4's move cleared, for the outcome and the task's event.
    mark_cleared: Option<MarkClearedFacts>,
    pick: Option<Pick>,
    renewal: Option<Renewal>,
    /// What the selection booked, for the outcome.
    reservation: Option<ReservationFacts>,
    /// Step 4's slow-down read of the pinned account, which a slow-down hold's outcome copies.
    slow_down: Option<SlowDownRead>,
    /// The slow-down hold step 5 answered, for its outcome's cycle.
    hold_plan: Option<HoldPlan>,
    /// Where the client's classifier call went (`judged_candidate`'s word, else `PLACED_DIRECTION`), for its Pin step.
    /// `None` for a call a fallback credit directed, whose `credit` fact says where it went.
    direction: Option<&'static str>,
    /// The words of the placement the fallback credit or the judged turn directed ahead of the strategy, for the
    /// summary's pick.
    directed: Option<DirectedWords>,
}

/// The pin a renewal kept, as the summary's renewal fact reads it: whether it was cold on this request, the cause
/// `warmth_of` named, and its epoch before the write.
struct Renewal {
    cold: bool,
    cause: &'static str,
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

    /// The fallback credit's directed placement shows under the `pin` step, which step 1 has already pushed: it gains
    /// the `credit` fact naming the account, so the record keeps its stations and its verdict words. Its verdict reads
    /// `match`, but on the client's classifier call (`classifier`), whose step keeps `none` since the call holds no
    /// pin.
    fn directed_placed(&mut self, account_id: &str, classifier: bool) {
        if let Some(pin) = self.walk.iter_mut().find(|w| w.step == "pin") {
            if !classifier {
                pin.verdict = "match";
            }
            pin.facts
                .push(Fact::text(CREDIT_WORDS.fact, account_id).about(account_id));
        }
    }

    /// The `pin` step of the client's classifier call, which pins nothing and keeps the verdict `none` beside its
    /// reason: the turn it judges by that turn's pin's agent and model, the account the judged turn ran on, the one the
    /// charge reads, or `none` in a session with no turn to judge, then where the call went. A call a fallback credit
    /// directed carries no `direction`, since the `credit` fact `directed_placed` wrote names where it went.
    fn judged_pin_step(&mut self, account_id: Option<&str>, turn: Option<&PinRef>) {
        let direction = self.direction;
        if let Some(pin) = self.walk.iter_mut().find(|w| w.step == "pin") {
            if let Some(turn) = turn {
                pin.facts.push(Fact::text("judged_agent", &turn.agent_id));
                pin.facts.push(Fact::text("judged_model", &turn.model));
            }
            pin.facts.push(match account_id {
                Some(account_id) => Fact::text(JUDGED_WORDS.fact, account_id).about(account_id),
                None => Fact::text(JUDGED_WORDS.fact, "none"),
            });
            pin.facts
                .extend(direction.map(|word| Fact::text("direction", word)));
        }
    }
}

fn run(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: Option<&PoolRow>,
    request: &SelectRequest,
    turn: Option<&JudgedTurn>,
    rng: &mut impl Rng,
    rec: &mut Recorder,
) -> (SelectResult, Vec<StateWrite>) {
    let mut writes = Vec::new();
    let Some(pool) = pool else {
        rec.pin_reason = Some("not_looked_up");
        rec.step("eligible", "none", Vec::new());
        return (SelectResult::Exhausted(Exhaustion::Untimed), writes);
    };
    let ttl_ms = affinity_ttl_ms(pool);
    // A marker-less request writes no cache, so under `when_cached` it has no pin to keep and spreads by the
    // strategy instead of pinning. Every request under `always`, a marked one and a non-portable one still pin, the
    // client's classifier call aside.
    let session = Some(request.session_id.as_str()).filter(|_| {
        !request.classifier
            && ttl_ms > 0
            && (pool.affinity.mode == AffinityMode::Always || request.marker || !request.portable)
    });
    // The task hands a failover's entry on until a selection lands the session. A request that takes no pin lands
    // nothing, so it reads no entry, and the entry waits for the session's next request that takes a pin. The copy
    // shadows `request`, so no read below can reach the entry.
    let unpinned;
    let request = match session {
        None if request.failed_over.is_some() => {
            unpinned = SelectRequest {
                failed_over: None,
                ..request.clone()
            };
            &unpinned
        }
        _ => request,
    };
    if request.classifier {
        // The client's classifier call follows its session's judged turn and holds no pin of its own.
        rec.pin_reason = Some("classifier");
    } else if session.is_none() {
        rec.pin_reason = Some("pinning_off");
    }

    // Step 1: the pin, expired when idle past the TTL or when its account can no longer serve the model here. A
    // non-portable request keeps a pin whose account left the pool: the fence below answers for that account,
    // because no other account holds the state the request refers to.
    let mut pin = None;
    let mut previous_account: Option<String> = None;
    let mut expired: Option<PinKey> = None;
    // The key step 1 reads under, whose row the placement's own fit and Pace score leave out of the account's burn.
    let mut own_key: Option<PinKey> = None;
    // The account of the row step 1 read, with what it hands on, whatever its `found`: a turn after an idle expiry that
    // lands back on the account reads its cache against that size.
    let mut row_size: Option<(String, (u64, bool, bool))> = None;
    let mut row_warmth: Option<(Warmth, WarmthCause)> = None;
    if let Some(session) = session {
        let key = request.pin_key();
        // A slot wait ends at the admission, the request's dispatch, which the relay names as the cap hold's end, so
        // the wait never lands in the pin's next gap.
        if let Some(until) = request
            .holds
            .iter()
            .filter(|hold| hold.reason == HoldReason::Cap)
            .filter_map(|hold| hold.until)
            .max()
        {
            state.pace_held_until(&key, until);
        }
        let own_row = state.pin(&key).cloned();
        // A turn after one on another of the session's models follows that turn's landing, which the own key's row
        // does not know. An auxiliary call keeps the own key's read, and the reader leaves out the pins only such
        // calls use, so an auxiliary call on another model is never a turn's previous landing. A failover's landing
        // finds no own row, since the failover took it, so the row it took stands in for it: an older row of another
        // model must not hide the failover's source.
        if !placed_auxiliary_only(request) {
            let own = own_row
                .as_ref()
                .or(request.failed_over.as_ref().map(|f| &f.row))
                .map(|p| (p.last_used_at, p.created_at));
            previous_account = state
                .session_latest_pin(
                    &request.key_id,
                    session,
                    &request.agent_id,
                    &request.canonical_model,
                )
                .filter(|latest| {
                    own.is_none_or(|stamp| (latest.last_used_at, latest.created_at) > stamp)
                })
                .map(|latest| latest.account_id.clone());
        }
        if let Some(mut current) = own_row {
            // Whatever `found` names the row, it says where the session's previous request landed, which the
            // placement compares against, unless a turn on another of the session's models landed since.
            previous_account.get_or_insert_with(|| current.account_id.clone());
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
            let mut marked = found == "live" && state.shed_marked(&key);
            // A live marked pin loses its mark here, ahead of the move rules, so the record's pin block and the
            // renewal below read the cleared row: released when its pool turned the shedding off, which no projection
            // can undo, so the switch is read before the heal check, else healed when its trigger reads gone.
            if marked {
                let cleared_as = if !load_shedding(pool) {
                    Some(("released", None))
                } else {
                    healed(state, snapshot, pool, &current, request.now)
                        .map(|heal| ("healed", Some(heal)))
                };
                if let Some((reason, heal)) = cleared_as {
                    if let Some(cleared) = state.clear_mark(&key) {
                        rec.mark_cleared = current
                            .mark
                            .as_ref()
                            .map(|mark| mark_cleared_of(mark, reason, heal.as_ref()));
                        writes.push(StateWrite::UpsertPin(cleared.clone()));
                        current = cleared;
                        marked = false;
                    }
                }
            }
            // Judged for every row, though only a live pin's verdict reaches the record and the rules below: a row that
            // idled out or left its pool never gets one.
            let judged = warmth_of(
                &current,
                request,
                &state.affinity().read().expect("affinity lock poisoned"),
            );
            row_warmth = Some(judged);
            row_size = handed_row(&current, request.pin_ended, judged.1, request.now)
                .map(|handed| (current.account_id.clone(), handed));
            // The row's own key, never `key`: the pace and the head mark read a key as the row spells it.
            let current_key = current.key();
            // A turn of a held class folds its arrival into the pin's cycle under the live row's key, which a move
            // and a failover's landing write their row under again. A request carrying a hold, whatever held it, is
            // a second selection whose arrival the first folded, and its gap of zero would drag the cycle toward zero.
            if found == "live"
                && slowable(
                    request,
                    snapshot.account(&current.account_id).map(|a| a.kind),
                )
                && request.holds.is_empty()
            {
                state.fold_pace(&current_key, request.received_at, burn_average_ms(pool));
            } else if found == "live"
                && request
                    .holds
                    .last()
                    .is_some_and(|hold| hold.reason == HoldReason::Rewrite)
            {
                // The head released the wait where it came, not at the bound, so the dispatch is this selection's
                // instant and the pin's next gap is measured from it.
                state.pace_held_until(&current_key, request.now);
            }
            rec.pin = Some(pin_facts(
                found,
                Some(&current),
                marked,
                request.now,
                state.pin_pace(&current_key),
            ));
            if found == "live" {
                pin = Some((current, current_key));
            } else {
                // The row stands until the placement replaces it: that placement may be held for a slot first, and
                // the selection after the wait reads the account the session left from this row.
                expired = Some(key.clone());
            }
        } else {
            rec.pin = Some(pin_facts(
                "none",
                None,
                false,
                request.now,
                state.pin_pace(&key),
            ));
        }
        own_key = Some(key);
    }

    // The fast answer names the pinned account's credits reason first, whatever becomes of the pin below.
    let pinned_account = pin.as_ref().map(|(p, _)| p.account_id.clone());

    // What the request says about the pin's cache, judged once on the row step 1 read: every rule below that reads
    // warmth reads this, so a compacted or restarted request is cold on the request that carries the fact.
    // Kept for a live pin alone, so the arms below that hold the pin hold its warmth too.
    let warmth = pin.as_ref().and(row_warmth);
    let verdict = warmth.map_or(Warmth::Untouched, |(verdict, _)| verdict);
    match rec.pin.as_mut() {
        Some(facts) => {
            if let Some((verdict, cause)) = warmth {
                facts.warmth = Some(verdict.as_str());
                facts.warmth_cause = Some(cause.as_str());
            }
            let line = pin_line(facts);
            rec.step("pin", if pin.is_some() { "match" } else { "none" }, line);
        }
        None => {
            // Only a request filtered off pinning reaches here, so the reason was set above.
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
    // Where step 1 left no live pin under the request's own key (no row, or one idle past the TTL or left behind by
    // its pool), the record's Pace figures are a placement's and leave the row out as `choose` does. A live pin's
    // renewal reads the row as every read other than a placement's own does.
    let placing_own = own_key.as_ref().filter(|_| pin.is_none());
    for candidate in &request.candidates {
        let blocking = state.blocking_cooldown(
            &candidate.account_id,
            &request.canonical_model,
            request.now,
            request.fast,
        );
        let found = obstacle(
            state,
            snapshot,
            pool,
            &candidate.account_id,
            blocking,
            &request.attempted,
            request.now,
            request.fallback_ms,
            request.fast,
        );
        let (facts, limits) = candidate_facts(
            state,
            snapshot,
            pool,
            request,
            candidate,
            window,
            blocking,
            found.as_ref(),
            placing_own,
        );
        rec.candidates.push(facts);
        rec.horizon_bounds.push(limits);
        standing.push(
            Fact::text(
                "standing",
                found.as_ref().map_or("eligible", Obstacle::name),
            )
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
    // A fallback credit token is good on the organization that issued it alone, so the body carrying one goes to that
    // account ahead of the moves and the strategy, which would place it where the upstream refuses the token. The
    // client's classifier call goes ahead of the strategy the same way, to the account its judged turn ended on, where
    // the upstream keeps it free, else to the account that turn's pin stands on now.
    let credit = credit_candidate(
        request,
        &eligible,
        pin.as_ref().map(|(current, _)| current.account_id.as_str()),
        state,
        pool,
    );
    let judged = judged_candidate(state, snapshot, request, turn, &eligible, window);
    let directed = credit
        .map(|c| (c, CREDIT_WORDS))
        .or(judged.map(|(c, _)| (c, JUDGED_WORDS)));
    rec.directed = directed.map(|(_, words)| words);
    if let Some(c) = credit {
        rec.directed_placed(&c.account_id, request.classifier);
    }
    if request.classifier {
        rec.direction = match (credit, judged) {
            (Some(_), _) => None,
            (None, Some((_, direction))) => Some(direction),
            (None, None) => Some(PLACED_DIRECTION),
        };
    }

    // A probe warms the pin it was sent for, so it never creates a pin or moves one onto a member no request asked
    // for: a probe that finds no live pin, or a pinned member that cannot serve while a sibling could take it, answers
    // that standing and places nothing. The keepalive reads the obstacle's name off the answer.
    if request.probe {
        let Some((current, _)) = pin.as_ref() else {
            let obstacle = PinnedObstacle::NoPin;
            rec.step(
                "probe",
                "answered",
                vec![Fact::text("obstacle", obstacle.as_str())],
            );
            let exhaustion = Exhaustion::Pinned { obstacle };
            return (SelectResult::Exhausted(exhaustion), writes);
        };
        let mut facts = Vec::new();
        let pinned = obstacles.iter().find(|(id, _)| id == &current.account_id);
        let tried = request.attempted.contains(&current.account_id);
        if let Some((_, o)) = pinned {
            facts.push(Fact::text("obstacle", o.name()));
        }
        facts.push(Fact::bool("tried", tried));
        let answered = |rec: &mut Recorder, facts: Vec<Fact>, exhaustion: Exhaustion| {
            rec.step("probe", "answered", facts);
            SelectResult::Exhausted(exhaustion)
        };
        // Every obstacle is named, so one added later fails to build here until it is given its answer to a probe.
        let standing = match pinned.map(|(_, o)| o) {
            // The window names the obstacle for the relay, which renders it on the wire as the overage claim.
            Some(Obstacle::Budget { until, .. }) => {
                let exhaustion = Exhaustion::Timed {
                    deadline: *until,
                    window: BUDGET_WINDOW.into(),
                };
                return (answered(rec, facts, exhaustion), writes);
            }
            // No wait clears these, so they answer untimed whatever the siblings hold, and the next real request
            // moves the pin if it must. With the member's spend unknown no deadline dates the answer.
            Some(Obstacle::BudgetUnknown) => Some(PinnedObstacle::BudgetUnknown),
            Some(Obstacle::Ended) => Some(PinnedObstacle::SubscriptionEnded),
            Some(Obstacle::Paused) => Some(PinnedObstacle::Operator),
            // With no sibling to take the probe no pin could move, so the pool's own exhaustion is the truer answer,
            // a cooldown's deadline among it.
            Some(_) if eligible.is_empty() => None,
            // A member this probe tried whose state moved since, as when the attempt's own mark reached the snapshot
            // before this selection, is still the member the probe tried.
            Some(Obstacle::Unready) if tried => Some(PinnedObstacle::Attempted),
            Some(Obstacle::Attempted) => Some(PinnedObstacle::Attempted),
            Some(Obstacle::Cooldown { until, .. }) => {
                Some(PinnedObstacle::Cooldown { until: *until })
            }
            Some(Obstacle::Credits { .. }) => Some(PinnedObstacle::Credits),
            Some(Obstacle::Deleted | Obstacle::Unready) => Some(PinnedObstacle::Operator),
            Some(Obstacle::Overage { until }) => Some(PinnedObstacle::Overage { until: *until }),
            Some(Obstacle::Spent { until, .. }) => Some(PinnedObstacle::Spent { until: *until }),
            None => None,
        };
        if let Some(obstacle) = standing {
            let exhaustion = Exhaustion::Pinned { obstacle };
            return (answered(rec, facts, exhaustion), writes);
        }
        rec.step("probe", "passed", facts);
    }

    // Step 3: a non-portable request is bound to the pin it holds. The client's classifier call holds none, so it goes
    // where the direction read above or the placement sends it.
    if let (false, Some(((current, current_key), warmth))) =
        (request.portable, pin.as_ref().zip(warmth))
    {
        if let Some(candidate) = candidate_in(&eligible, &current.account_id) {
            if let Some(hold) = rewrite_answer(state, pool, request, current, current_key) {
                return (SelectResult::Held(hold), writes);
            }
            rec.step("bound", "renewed", vec![account_fact(&current.account_id)]);
            return (
                SelectResult::Selected(renew(
                    state,
                    snapshot,
                    pool,
                    current,
                    current_key,
                    candidate,
                    request,
                    warmth,
                    previous_account.as_deref(),
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
        let exhaustion = fenced(state, snapshot, pool, request, &current.account_id, rec);
        return (SelectResult::Exhausted(exhaustion), writes);
    }
    // Step 4: the moves, for a portable request with a live pin. A pin on an account past its saved end never
    // reaches them: step 5 releases it as it releases a cooled account's. A token count follows its pin: it
    // creates no pin (step 7), so a move it made would leave the session with none. A probe follows its pin too,
    // so a rule that would move it, a standing mark's among them, waits for the session's next real request.
    let mut migrated = None;
    let mut shed_from = None;
    // The row a move takes off its source, whose burn and last prompt the new row carries.
    let mut moving: Option<PinRow> = None;
    let mut selection_set: Vec<&Candidate> = eligible.clone();
    // The fit reads whose runway breaks the strategy's tie: a move's from its rule's gate, a placement's from the
    // `no_fit` partition, which keeps the reads of the candidates that fit. `choose` reads them for a move or a
    // failed-over request, never for a fresh session.
    let mut reads: Vec<(String, FitRead)> = Vec::new();
    if let Some((mut current, current_key)) = pin.take() {
        // The move rules, the booking and the row a move takes off read the session's thinking sums as this selection
        // writes them, so a compacted request prices no drop of blocks its prompt no longer holds.
        let as_written = |mut row: PinRow| {
            if zeroes_sums(request) {
                for sum in row.thinking_sums.values_mut() {
                    sum.tokens = 0;
                }
            }
            row
        };
        current = as_written(current);
        let mut moved_off = false;
        let ended = subscription_ended(snapshot, &current.account_id, request.now);
        if request.count || request.probe || ended {
            let reason = if request.count {
                "count"
            } else if request.probe {
                "probe"
            } else {
                "subscription_ended"
            };
            for rule in MigrateReason::ORDER {
                rec.step(rule.step(), "not_run", vec![Fact::text("reason", reason)]);
            }
        } else if let Some((candidate, words)) = directed {
            for rule in MigrateReason::ORDER {
                rec.step(
                    rule.step(),
                    "not_run",
                    vec![Fact::text("reason", words.reason)],
                );
            }
            if candidate.account_id == current.account_id {
                // The pin renews in place at step 5, which reads the account's pace as a pinned turn does.
                rec.slow_down =
                    slow_down_read(state, snapshot, pool, &current.account_id, request.now);
            } else {
                // Handed to the placement as a fired move hands its source, so the new row carries its burn and its
                // last prompt. No move reason names a credit, so the write reads `created`.
                state.remove_pin(&current_key);
                writes.push(StateWrite::DeletePin(current_key.clone()));
                if let Some(mark) = standing_mark(&current) {
                    rec.mark_cleared = Some(mark_cleared_of(mark, "moved", None));
                }
                moving = Some(current.clone());
                moved_off = true;
            }
        } else {
            // The account's pace, read and written once ahead of the rules, so the shed rule and the hold read one
            // figure.
            let slowed = slow_down_read(state, snapshot, pool, &current.account_id, request.now);
            let cold = !warm_on(&current, verdict, request.now);
            // The lane the pin carries once this request renews it: a request that leaves the pin alone stamps
            // none.
            let fast = if verdict == Warmth::Untouched {
                lane_is_fast(&current.lane)
            } else {
                request.fast
            };
            let moved = move_of(
                state,
                snapshot,
                pool,
                request,
                &current,
                &current_key,
                &eligible,
                window,
                cold,
                fast,
                slowed.as_ref(),
                rec,
            );
            // A round marks the state's rows, the request's own among them, so the row the renewal writes back or
            // the move takes off the account is read again with its mark.
            if rec.shed_round.is_some() {
                if let Some(fresh) = state.pin(&current_key).cloned() {
                    current = as_written(fresh);
                }
            }
            // Ahead of the move's own writes, so the store holds the marks before the pin write that spends one.
            if let Some(round) = rec.shed_round.as_ref() {
                writes.extend(round_write(state, pool, &current, request.now, round));
            }
            rec.slow_down = slowed;
            // A spent window about to reset holds the request on its own account: the hold books and writes
            // nothing, the relay waits to the later of the reset and the cooldown's end and selects again, and the
            // pin renews in place then.
            if let MoveVerdict::Wait { until } = moved {
                state.pace_held_until(&current_key, until);
                let hold = hold_facts(
                    request,
                    &current.account_id,
                    HoldReason::Reset,
                    Some(until),
                    None,
                );
                return (SelectResult::Held(hold), writes);
            }
            // A hold is for a fresh placement: with every destination at its cap the move is not made, the pin
            // renews in place at step 5 while its account stays eligible (a cooled one is released there and the
            // request placed afresh), and the next request judges the move again.
            if let MoveVerdict::Move(reason, set, move_reads) = moved {
                let cap = pool.failover.in_flight_cap;
                let loads: Vec<u32> = set
                    .iter()
                    .map(|c| state.in_flight().account(&c.account_id))
                    .collect();
                // The verdict reads the counts the step prints, so a count that moves mid-selection cannot leave a
                // record that contradicts its own verdict.
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
                    state.remove_pin(&current_key);
                    writes.push(StateWrite::DeletePin(current_key.clone()));
                    // The mark leaves with the row the move takes off the account it was made on.
                    if let Some(mark) = standing_mark(&current) {
                        rec.mark_cleared = Some(mark_cleared_of(mark, "moved", None));
                    }
                    if reason == MigrateReason::Shed {
                        shed_from = Some(current.account_id.clone());
                    }
                    rec.rule = Some(reason);
                    rec.source = Some(current.account_id.clone());
                    rec.destinations = set.iter().map(|c| c.account_id.clone()).collect();
                    migrated = Some(reason);
                    selection_set = set;
                    reads = move_reads;
                    moving = Some(current.clone());
                    moved_off = true;
                }
            }
        }
        if !moved_off {
            pin = Some((current, current_key));
        }
    }

    // Step 5: a live pin whose account is eligible is renewed; stickiness wins over priority.
    // A slowed pin waits before it renews. A pin whose account left the set takes the placement path below, whatever
    // deadline stands on it.
    if let Some(((current, current_key), warmth)) = pin.as_ref().zip(warmth) {
        // Step 1 read the pace before step 4's read could drop its deadline at a passed reset, so the record names the
        // deadline the pace holds once step 4 has read the account.
        if let Some(facts) = rec.pin.as_mut() {
            facts.slow_down_until = state
                .pin_pace(current_key)
                .and_then(|pace| pace.standing_deadline(request.now));
        }
        if let Some(candidate) = candidate_in(&eligible, &current.account_id) {
            if let Some(hold) = rewrite_answer(state, pool, request, current, current_key) {
                return (SelectResult::Held(hold), writes);
            }
            if let Some(hold) =
                slow_down_answer(state, snapshot, pool, request, current, current_key, rec)
            {
                return (SelectResult::Held(hold), writes);
            }
            return (
                SelectResult::Selected(renew(
                    state,
                    snapshot,
                    pool,
                    current,
                    current_key,
                    candidate,
                    request,
                    warmth,
                    previous_account.as_deref(),
                    &mut writes,
                    rec,
                )),
                writes,
            );
        }
    }

    // Step 6: a model with its own window needs allowance left in it and in both shared windows, the cap bounds a
    // fresh placement, `no_fit` prefers the accounts the request fits to every horizon, and the wind-down prefers
    // accounts that can take a fresh session. Each preference falls through to the wider set it narrowed when it
    // would otherwise empty the selection, and every candidate a partition drops joins the trace under that
    // partition's name.
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
    // The allowance and fit reads, each by account, which the placement's rungs start from rather than reading the
    // same state again. The spent partition reads every allowance off `seven_day`, so the build runs there alone. The
    // fit's ladder orders its last rungs by the allowance and reads one the build left out on demand.
    let allowances: Vec<(&str, bool)> = if window != SEVEN_DAY {
        selection_set
            .iter()
            .copied()
            .map(|c| {
                let holds = holds_allowance(state, snapshot, &c.account_id, window, request.now);
                (c.account_id.as_str(), holds)
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut verdicts: Vec<(&str, FitWord)> = Vec::new();
    let mut partitions = Vec::with_capacity(5);
    if window != SEVEN_DAY {
        let mut left: Vec<&Candidate> = Vec::new();
        let mut dropped = 0;
        for (c, &(_, holds)) in selection_set.iter().copied().zip(&allowances) {
            if holds {
                left.push(c);
                continue;
            }
            if let Some((until, spent)) =
                spent_until(state, snapshot, &c.account_id, window, request.now)
            {
                spent_deadlines.push((
                    c.account_id.clone(),
                    Obstacle::Spent {
                        until,
                        window: spent.to_string(),
                    },
                ));
            }
            // Extra usage funds every model, so under `allow` a spent window goes on in overage unless the poll read
            // the account's extra usage as blocked. A state the poll has not read is no refusal.
            let funded = pool.overage_policy == OveragePolicy::Allow
                && state
                    .credits_blocked(snapshot, &c.account_id, request.now)
                    .is_none();
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
    // preferences below and a free account the fit would pass over still takes the request. With every
    // account that could serve at its cap the request is held for a slot; the set is left whole so the answer
    // names the account the strategy would have chosen. A pinned request never gets here: step 5 renewed it.
    // A token count is bound to no cap: it occupies no slot (the task charges none for it), so the cap it would
    // be held for is one it never fills. It keeps the whole set and lands where its pin or the strategy says.
    let free: Vec<&Candidate> = selection_set
        .iter()
        .copied()
        .filter(|c| request.count || under_cap(state, pool, &c.account_id))
        .collect();
    let held = free.is_empty();
    let at_cap = if held && !selection_set.is_empty() {
        Fact::text("at_cap", "held")
    } else {
        narrow(&mut selection_set, &mut skipped, free, "at_cap", rec)
    };
    partitions.push(at_cap);
    // The fit is a preference, not a refusal: with no account fitting the request goes where the strategy picks
    // rather than answering exhausted while capacity remains. A move's destinations are the set its rule's gate
    // ordered, read with the moving pin already, so they stay as the gate left them with the gate's reads. Every other
    // landing reads each candidate with the fresh figures, where one inside the wind-down never fits, and walks one
    // ladder whose rung is the `ladder` fact: the fitting candidates, else the `rewrite_only` ones with the longest
    // runway to the ceiling, else the `least_bad` ones holding allowance with the longest runway to 1.0, else the ones
    // inside their wind-down holding allowance, else the whole set. `longest` reads across priorities, so a lower
    // priority with the longer runway takes the landing. A `least_bad` or winding-down member with no allowance left
    // stands on the last rung alone, where the whole set stands rather than an exhaustion on the router's own estimate.
    // The partition keeps the read of each candidate that fits, which breaks the strategy's tie for a failed-over
    // request.
    let mut ladder = None;
    // Built once for the fit below, the placement's rung 4 and the booking, since nothing between them changes a pin.
    let prior = match moving {
        Some(_) => Vec::new(),
        None => fresh_prior(state, snapshot, pool, request.now),
    };
    let fitting: Vec<&Candidate> = if moving.is_some() {
        selection_set.clone()
    } else {
        let mut fitting = Vec::with_capacity(selection_set.len());
        let mut rewrite_only: Vec<(&Candidate, FitRead)> = Vec::new();
        let mut least_bad: Vec<(&Candidate, FitRead)> = Vec::new();
        let mut ending: Vec<&Candidate> = Vec::new();
        for c in selection_set.iter().copied() {
            let read = fit_of(
                state,
                snapshot,
                pool,
                request,
                Figures::Fresh(&prior),
                &c.account_id,
                &[],
                request.now,
                Judge::Placement,
                own_key.as_ref(),
            );
            // A `least_bad` row states the runway to 1.0, the one its rung ranks by.
            let (runway_ms, binding) = if read.verdict == FitWord::LeastBad {
                (read.wall_shown_ms, read.wall_binding.clone())
            } else {
                (read.runway_ms, read.binding.clone())
            };
            if let Some(member) = rec.candidate_mut(&c.account_id) {
                member.runway_ms = runway_ms;
                member.fit = Some(read.verdict.as_str());
                member.binding = binding;
            }
            verdicts.push((&c.account_id, read.verdict));
            let holds = || {
                read_of(&allowances, &c.account_id).unwrap_or_else(|| {
                    holds_allowance(state, snapshot, &c.account_id, window, request.now)
                })
            };
            match read.verdict {
                FitWord::Fits => {
                    fitting.push(c);
                    reads.push((c.account_id.clone(), read));
                }
                FitWord::RewriteOnly => rewrite_only.push((c, read)),
                FitWord::LeastBad if holds() => least_bad.push((c, read)),
                FitWord::Ending if holds() => ending.push(c),
                _ => {}
            }
        }
        fn longest_of<'a>(
            rung: &[(&'a Candidate, FitRead)],
            runway: impl Fn(&FitRead) -> Option<i64>,
        ) -> Vec<&'a Candidate> {
            let rung = rung.iter().map(|(c, read)| (*c, read)).collect();
            longest(rung, runway).into_iter().map(|(c, _)| c).collect()
        }
        let (rung, kept) = if !fitting.is_empty() {
            (FitWord::Fits.as_str(), fitting)
        } else if !rewrite_only.is_empty() {
            (
                FitWord::RewriteOnly.as_str(),
                longest_of(&rewrite_only, |read| {
                    read.rung_runway_ms(FitWord::RewriteOnly)
                }),
            )
        } else if !least_bad.is_empty() {
            (
                FitWord::LeastBad.as_str(),
                longest_of(&least_bad, |read| read.rung_runway_ms(FitWord::LeastBad)),
            )
        } else if !ending.is_empty() {
            (FitWord::Ending.as_str(), ending)
        } else {
            ("every", Vec::new())
        };
        // A walk over no member takes no rung, so its record names none.
        ladder = (!selection_set.is_empty()).then_some(rung);
        kept
    };
    partitions.push(narrow(
        &mut selection_set,
        &mut skipped,
        fitting,
        "no_fit",
        rec,
    ));
    if let Some(rung) = ladder {
        partitions.push(Fact::text("ladder", rung));
    }
    // A session should not start on an account about to end.
    let not_winding_down: Vec<&Candidate> = selection_set
        .iter()
        .copied()
        .filter(|c| !winding_down(snapshot, pool, &c.account_id, request.now))
        .collect();
    partitions.push(narrow(
        &mut selection_set,
        &mut skipped,
        not_winding_down,
        "winding_down",
        rec,
    ));
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
    let placed = match directed {
        Some(_) => None,
        None => placement_of(
            state,
            snapshot,
            pool,
            request,
            moving.as_ref(),
            &prior,
            &affinity_guard,
            &allowed,
            window,
            own_key.as_ref(),
            &allowances,
            &verdicts,
            rec,
        ),
    };
    let (chosen, tier, placement) = if let Some((candidate, words)) = directed {
        // Every station the console draws is recorded, the rungs and the pick naming the credit or the judged turn as
        // their reason.
        for rung in ["rung:content", "rung:with_main", "rung:static"] {
            rec.step(rung, "none", vec![Fact::text("reason", words.reason)]);
        }
        rec.step(
            "pick",
            "chosen",
            vec![
                account_fact(&candidate.account_id),
                Fact::text("reason", words.reason),
            ],
        );
        (candidate, None, None)
    } else if let Some((candidate, placement)) = placed {
        (candidate, None, Some(placement))
    } else {
        // The runway breaks the strategy's tie for a move and a failed-over request alone: a fresh session, held at
        // the cap or not, keeps the strategy's own draw among its ties, which spreads new sessions over them.
        let tie_reads: &[(String, FitRead)] = if moving.is_some() || request.failed_over.is_some() {
            &reads
        } else {
            &[]
        };
        let Some((strategy_choice, tier)) = choose(
            state,
            snapshot,
            pool,
            window,
            &selection_set,
            tie_reads,
            &spent_model,
            request.now,
            request.fallback_ms,
            !held,
            own_key.as_ref(),
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
            let session_pin =
                any_agent_pin(state, snapshot, request, &allowed, window, ttl_ms, rec);
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
                let hold = HoldFacts {
                    cohort: cohort.map(|c| c.account_id.clone()),
                    ..hold_facts(
                        request,
                        &cohort.unwrap_or(strategy_choice).account_id,
                        HoldReason::Cap,
                        None,
                        None,
                    )
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
                return (SelectResult::Held(hold), writes);
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

    // A move or a fresh placement releases the request's previous booking first, so a failover's booking replaces it
    // on a priced destination and the source's leaves even where the destination books nothing, else the source
    // would carry the rewrite and burn of a stream it no longer serves until the usage, the end or the expiry. A
    // retry in place renews and never reaches here, so its booking stands. A probe never reaches here either, since it
    // stays on its live pin or answers exhausted. Then the selection books its rewrite and burn on the destination
    // until the usage replaces it or the request ends. A count writes no cache, a fast request draws usage credits and
    // no plan window, and the client's classifier call takes no share of a plan window on its judged turn's account
    // and charges at its usage alone elsewhere, so none of them books. A destination without a tier prices nothing,
    // and nothing priced is nothing to book, so an unpriced destination leaves no entry to expire or release.
    state.release_reservation(&request.request_id);
    if !(request.count || request.fast || request.classifier) {
        if let Some(tier) = snapshot.priced_tier(&chosen.account_id) {
            let (rewrite, burn, regime) = pin_figures(
                state,
                pool,
                &chosen.account_id,
                request,
                Figures::of(moving.as_ref(), &prior),
                tier,
                request.now,
            );
            if !(rewrite.is_empty() && burn.is_empty()) {
                let reservation = Reservation {
                    account_id: chosen.account_id.clone(),
                    attempt: request.attempt,
                    pin: session.map(|_| request.pin_key()),
                    model: request.canonical_model.clone(),
                    entrypoint: regime.map(String::from),
                    booked_at: request.now,
                    rewrite,
                    burn,
                    expires_at: request.now + IN_FLIGHT_RECLAIM_MS,
                };
                let (step, facts) = reserve_step(&reservation);
                rec.step(step.step, step.verdict, step.facts);
                rec.reservation = Some(facts);
                state.book_reservation(&request.request_id, reservation);
            }
        }
    }

    let left = previous_account
        .or_else(|| {
            request
                .failed_over
                .as_ref()
                .map(|f| f.from_account_id.clone())
        })
        .filter(|account| account != &chosen.account_id);
    // Step 7: a fresh pin with a fresh epoch. A token count touches no cache, so it creates none: a pin it left
    // on an account at its cap would carry the agent's next message past the cap, since a renewal never reads it.
    let handed = session.filter(|_| !request.count).map(|session| {
        let epoch = state.fresh_epoch();
        // A failover's landing carries the row its source lost and remembers the source as a shed move does, and a
        // deleted account's landing carries the row alone.
        let failed_over = request.failed_over.as_ref();
        let moving = moving.as_ref().or(failed_over.map(|f| &f.row));
        // A live row released off its account, or one idle past the TTL or left behind by its pool, is the session's
        // too, so the thinking it carries follows the session onto the new row.
        let replaced = moving
            .or(pin.as_ref().map(|(row, _)| row))
            .or(expired.as_ref().and_then(|key| state.pin(key)));
        // A placement no rule made still moves the session where it lands a failover on the row its source lost, under
        // the failover's word, or where it replaces a live pin on another account, under the first word of
        // `MOVE_CAUSE_ORDER` the pin's list holds. A pin idle past the TTL expired, and so did one replaced on its own
        // account.
        rec.moved = match failed_over {
            Some(failed) => Some((failed.reason, failed.from_account_id.clone())),
            None => replaced
                .filter(|row| {
                    migrated.is_none()
                        && row.account_id != chosen.account_id
                        && !rec.released.contains(&"expired")
                })
                .map(|row| (move_word(&rec.released), row.account_id.clone())),
        };
        let row = fresh_pin(
            request,
            snapshot,
            session,
            epoch,
            chosen,
            shed_from
                .clone()
                .map(|from| (from, SHED_ROUND.to_string()))
                .or_else(|| {
                    failed_over
                        // The account the stamp would name is gone.
                        .filter(|f| f.reason != "deleted")
                        .map(|f| (f.from_account_id.clone(), f.reason.to_string()))
                }),
            moving,
            replaced,
        );
        rec.pin_written = Some(PinWritten {
            action: if migrated.is_some() {
                "moved"
            } else {
                "created"
            },
            epoch,
            lane: row.lane.clone(),
            refresh_stamped: true,
            hashes_stamped: request.static_hash().is_some() || !request.boundaries.is_empty(),
        });
        replace_expired(state, expired.take(), &mut writes);
        state.put_pin(row.clone());
        // The landing writes the destination's cache, so the pin's next requests wait for this request's head.
        if left.is_some() {
            state.mark_head(
                &row.key(),
                &request.request_id,
                request.attempt,
                request.now,
            );
        }
        let handed = pin_ref(&row);
        writes.push(StateWrite::UpsertPin(row));
        handed
    });
    let mut trace_skipped: Vec<Skipped> = obstacles
        .iter()
        .map(|(account_id, o)| Skipped {
            account_id: account_id.clone(),
            obstacle: o.name(),
        })
        .collect();
    trace_skipped.extend(skipped);
    if placement.is_some() || directed.is_some() {
        // Rung 3 and `with_main` choose from `allowed` and a directed placement from the eligible set, each ahead of
        // step 6's own partitions, so the account they land on can already carry a partition's label here. The
        // placement itself is proof enough that it serves.
        trace_skipped.retain(|s| s.account_id != chosen.account_id);
    }
    let row_handed = row_size
        .filter(|(account, _)| *account == chosen.account_id)
        .map(|(_, handed)| handed);
    (
        SelectResult::Selected(Selection {
            account: selected_row(snapshot, &chosen.account_id),
            upstream_model: chosen.upstream_model.clone(),
            pin: handed,
            migrated,
            in_overage: state.in_overage(&chosen.account_id, request.now, request.fallback_ms),
            shared_windows: shared_windows(state, &chosen.account_id),
            trace: SelectionTrace {
                strategy: pool.strategy.as_str(),
                tier,
                renewed: false,
                moved: migrated
                    .map(MigrateReason::as_str)
                    .or(rec.moved.as_ref().map(|(word, _)| *word)),
                skipped: trace_skipped,
                placement,
                cohort_hold,
            },
            pool_id: pool.id.clone(),
            left,
            judged: None,
            previous_input_tokens: row_handed.map(|(size, _, _)| size),
            entry_expired: row_handed.map(|(_, expired, _)| expired),
            agent_released: row_handed.map(|(_, _, released)| released),
        }),
        writes,
    )
}

/// The size a selection that lands on the account of the pin row step 1 read hands on: the row's
/// `last_input_tokens`, none for a row that has seen no usage report and for a turn `warmth_of` reads as compacted, on
/// another lane or restarted, whose cache the expiry count must not read against the size.
fn handed_size(last_input_tokens: u64, cause: WarmthCause) -> Option<u64> {
    match cause {
        WarmthCause::Compacted | WarmthCause::LaneChanged | WarmthCause::StaticRestart => None,
        _ => (last_input_tokens > 0).then_some(last_input_tokens),
    }
}

/// What a selection that lands on the account of the pin row step 1 read hands on to the expiry count: the size as
/// `handed_size` answers it, whether the row's entry was past its lifetime at `now`, and `ended`, whether the pin
/// carried `ended` when the routing task took the request. The row no longer tells that: the task clears it ahead of
/// the selection. `None` wherever the size is.
fn handed_row(
    row: &PinRow,
    ended: bool,
    cause: WarmthCause,
    now: i64,
) -> Option<(u64, bool, bool)> {
    handed_size(row.last_input_tokens, cause).map(|size| (size, !warm(row, now), ended))
}

/// Takes the row step 1 found idle past the TTL or left behind by its pool off the state, ahead of the pin that
/// replaces it under its key, as a move takes its source's row. The session's pace ends with that row, so the landing
/// starts with none rather than reading the idle time as one long gap.
fn replace_expired(
    state: &mut RoutingState,
    expired: Option<PinKey>,
    writes: &mut Vec<StateWrite>,
) {
    let Some(key) = expired else {
        return;
    };
    if let Some(old) = state.remove_pin(&key) {
        state.drop_pace(&old.key());
    }
    writes.push(StateWrite::DeletePin(key));
}

/// A pin under `session` on `chosen`, created by this request with the request's hashes stamped. `shed` is the source
/// and the word a shed move or a failover's landing stamps, `None` on a deleted account's landing. `moving` is the row
/// a move takes off its source, or a failover or the purge of a deleted account took off its account, whose burn
/// averages, the classifier copy's model and the last prompt the new row carries, so the destination's projection
/// counts the pin from the moment it lands, the copy keeps its price and a later move prices its rewrite from a figure.
/// `replaced` is any row of the pin the placement replaces, `moving` among them, whose thinking sums of the holders
/// `snapshot` carries the new row takes on.
#[allow(clippy::too_many_arguments)]
fn fresh_pin(
    request: &SelectRequest,
    snapshot: &RegistrySnapshot,
    session: &str,
    epoch: u64,
    chosen: &Candidate,
    shed: Option<(String, String)>,
    moving: Option<&PinRow>,
    replaced: Option<&PinRow>,
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
        // The rates are typed tokens per second and carry no unit, so the copy needs no re-pricing.
        burn: moving.map_or_else(PinBurn::default, |m| m.burn),
        burn_at: moving.and_then(|m| m.burn_at),
        burn_banded: moving.map_or_else(PinBurn::default, |m| m.burn_banded),
        burn_fast: moving.map_or_else(PinBurn::default, |m| m.burn_fast),
        burn_unmetered: moving.map_or_else(PinBurn::default, |m| m.burn_unmetered),
        burn_unmetered_model: moving.and_then(|m| m.burn_unmetered_model.clone()),
        last_input_tokens: moving.map_or(0, |m| m.last_input_tokens),
        // A fresh pin has no cache to be cold about, and its TTL is the default until the first report.
        last_refresh_at: request.now,
        cache_ttl_ms: FIVE_MINUTES_MS,
        // A shed move and a failover's landing remember the source for the dwell, so no round sends the session
        // straight back, and the word the status page and the record name the stamp by. Every other move clears it, a
        // deleted account's landing among them.
        shed: shed.map(|(from, reason)| ShedStamp {
            from,
            at: request.now,
            reason: Some(reason),
        }),
        static_hash: None,
        tail_hash: None,
        lane: request.lane.clone(),
        entrypoint: request.entrypoint.clone(),
        mark: None,
        auxiliary_only: placed_auxiliary_only(request),
        effort: request.effort.clone(),
        ended: false,
        thinking_sums: replaced
            .map(|r| handed_sums(r, request, snapshot))
            .unwrap_or_default(),
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
    rec: &mut Recorder,
) -> Exhaustion {
    let own = obstacle_on(
        state,
        snapshot,
        pool,
        account_id,
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

pub(crate) const FIVE_HOUR_MS: i64 = 5 * HOUR_MS;

/// The safety wait: the stretch has to read over the threshold at a pinned selection and still read over it at one
/// this much later before the round fires, so one large turn's charge does not fire a round on its own.
pub const SLOW_DOWN_WAIT_MS: i64 = 120_000;

/// A slow-down hold under a second is not served: the relay's tick is 250 ms and a second selection costs more than
/// the hold buys, so the pin renews unslowed and the next request reads again.
pub const SLOW_DOWN_MIN_HOLD_MS: i64 = 1_000;

/// The least share of the rest a fallback mark's burn covers: a pin under it relieves too little of the rest to be
/// worth its move, so it stays under `sliver`. A tenth of the rest, warm or cold. The round's guard reads the same
/// tenth of the round's burn sum: a mark that would leave the account's other unmarked priced pins under it is
/// refused as `last_pin`, so no round takes nearly all of an account's burn.
pub const SHED_MARK_SHARE: f64 = 0.1;

/// The share of a moved pin's own burn over the time to the horizon that a move's fit holds under the ceiling, so a
/// destination whose room exceeds that burn by less than half of it reads short for the move. The shed rule's gate
/// refuses such a destination, the round's destination pick refuses it, and the spent, overage, return-to-plan and
/// drain gates drop it to the rewrite-only order, which they take only where no sibling fits. A fresh placement keeps
/// the ceiling. One half, since the burn the recorded projection implies reads a median 1.07 times the decayed average
/// a pin's burn is priced from, p10 0.62 and p90 2.81, so a pin's burn under-reads by more than half in at least
/// one record in ten.
pub const SHED_FIT_BAND: f64 = 0.5;

/// Milliseconds as the seconds a share-per-second burn multiplies.
fn seconds(ms: i64) -> f64 {
    ms as f64 / 1_000.0
}

/// The projection of one account on one window at its horizon, read at `now`: the prediction plus what the burn adds
/// to the horizon, the band's seconds weighed as the band weighs them.
pub(crate) struct Projection {
    pub predicted: f64,
    /// Share per second at `now`: the account's `burn_shares` on the window under `BurnRead::Achieved`, its
    /// `demand_shares` under `BurnRead::Demand`.
    pub burn: f64,
    pub horizon: i64,
    /// The line the round, the heal and the renewal summary test `at_horizon` against.
    pub limit: f64,
    pub at_horizon: f64,
    /// The same burn over `[now, horizon)` as the band weighs it, whose increase `at_horizon` adds.
    pub banded: Banded,
    /// The reading's reset as `horizon_of` read it, `None` where the reading names none: a runway that reaches past it
    /// reads unbounded for the displayed figures, since the window resets before the ceiling is reached.
    pub reset: Option<i64>,
}

/// The pins the account's pool keeps: a pin under a key of a pool the account left is retained until it ends and
/// counts in neither a round nor a projection, so a former pool's pins never move this pool's figures. The keep leaves
/// an ended agent's pin out of every figure as well, until the agent's next request.
fn in_pool<'a>(snapshot: &'a RegistrySnapshot, pool: &'a PoolRow) -> impl Fn(&PinRow) -> bool + 'a {
    in_pool_without(snapshot, pool, None)
}

/// The pool's pins without the request's own, the predicate a placement's own reads hand to the sums. `own` is the
/// request's pin key under `Judge::Placement` and `None` on every other read. A pin whose agent ended is out, since
/// its agent sends no new turn: its burn would hold up a projection nothing will fill, and marking it relieves
/// nothing.
fn in_pool_without<'a>(
    snapshot: &'a RegistrySnapshot,
    pool: &'a PoolRow,
    own: Option<&'a PinKey>,
) -> impl Fn(&PinRow) -> bool + 'a {
    move |p: &PinRow| {
        !p.ended
            && keyed_in(snapshot, pool, p)
            && own.is_none_or(|own| p.key_parts() != own.parts())
    }
}

/// The pool's pins whose agent ended, which the keep leaves out of every figure. A round lists them in its record.
fn ended_in_pool<'a>(
    snapshot: &'a RegistrySnapshot,
    pool: &'a PoolRow,
) -> impl Fn(&PinRow) -> bool + 'a {
    move |p: &PinRow| p.ended && keyed_in(snapshot, pool, p)
}

/// A pin counts in its key's pool, so a former pool's pins never move this pool's figures.
fn keyed_in(snapshot: &RegistrySnapshot, pool: &PoolRow, p: &PinRow) -> bool {
    snapshot
        .key(&p.key_id)
        .is_some_and(|k| k.pool_id == pool.id)
}

/// What set a projection's horizon: the account's end and the window's own reset discard the quota left at them, so a
/// round tests the wall there, and no reset is known at the five-hour bound, so the quota reads on and the round tests
/// the ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HorizonBound {
    Wall,
    Span,
}

impl HorizonBound {
    /// The line a projection at a horizon this bound set reads over or under: the wall, 1.0, or the window's ceiling.
    fn limit(self, pool: &PoolRow, window: &str) -> f64 {
        match self {
            HorizonBound::Wall => 1.0,
            HorizonBound::Span => ceiling(pool, window),
        }
    }
}

/// The 5-hour horizon is the earlier of the reading's reset (`now + FIVE_HOUR_MS` for a reading past its reset with
/// none, since the window opens with the next request and runs five hours) and the account's end, the weekly horizon
/// the earliest of `now + FIVE_HOUR_MS`, the reset and the end. An end at or before `now` makes the horizon `now`. The
/// bound names which of them set it: an end or a weekly reset tied with the five-hour bound reads `Wall`, since the
/// tied instant still discards the quota left at it.
pub(crate) fn horizon_of(
    window: &str,
    reading: &WindowReading,
    end: Option<i64>,
    now: i64,
) -> (i64, HorizonBound) {
    let length = now.saturating_add(FIVE_HOUR_MS);
    let (horizon, bound) = match reading.reset {
        Some(reset) if window == FIVE_HOUR || reset <= length => (reset, HorizonBound::Wall),
        _ => (length, HorizonBound::Span),
    };
    let (horizon, bound) = match end {
        Some(end) if end <= horizon => (end, HorizonBound::Wall),
        _ => (horizon, bound),
    };
    (horizon.max(now), bound)
}

/// The instant the burn `projection` implies over the time to its horizon, `(at_horizon - predicted) / (horizon -
/// now)`, takes the window from its prediction to the wall, 1.0. `None` where that burn reads at or under zero, or
/// where the horizon is at or before `now`, which leaves no span to read a burn over. A tiny burn's far wall saturates.
fn wall_of(projection: &Projection, now: i64) -> Option<i64> {
    if projection.horizon <= now {
        return None;
    }
    let burn = (projection.at_horizon - projection.predicted) / (projection.horizon - now) as f64;
    (burn > 0.0).then(|| now.saturating_add(((1.0 - projection.predicted) / burn).round() as i64))
}

/// Whether a runway of `runway_ms` from `now` reaches past `reset`: such a runway is displayed as unbounded.
pub(crate) fn passes_reset(runway_ms: i64, reset: Option<i64>, now: i64) -> bool {
    reset.is_some_and(|reset| now.saturating_add(runway_ms) > reset)
}

/// The milliseconds the banded `burn` takes to carry `prediction` to `limit`, the one runway a window and a pin read:
/// `Some(0)` at or past the limit, since an idle spent window has none left, `None` with no burn, and `None` where the
/// runway reaches its wall, since the quota resets or the account ends before the limit. The wall is the earliest of
/// the window's `reset` and the account's `end` that lies after `now`, each instant tested by itself, so an end already
/// passed cannot hide a reset that lies ahead.
pub(crate) fn runway(
    prediction: f64,
    burn: &Banded,
    limit: f64,
    now: i64,
    reset: Option<i64>,
    end: Option<i64>,
) -> Option<i64> {
    if prediction >= limit {
        return Some(0);
    }
    let runway_ms = burn.time_to(limit - prediction)?;
    let wall = reset.into_iter().chain(end).filter(|&at| at > now).min();
    if wall.is_some_and(|wall| now.saturating_add(runway_ms) >= wall) {
        return None;
    }
    Some(runway_ms)
}

/// Which burn a projection reads: the achieved burn, what the upstream sees (the weekly trigger, the reported figures),
/// or the demand, the achieved burn over the account's realized speed (the fit gate, the slow-down).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BurnRead {
    Achieved,
    Demand,
}

/// The projections of `account_id` on the plan windows it holds a reading of, from one read of its burn as `read`
/// names it: empty for an api_key account or a usage-based seat, neither of which has a plan window. An unknown tier
/// prices no burn, so an untiered account projects its prediction alone. The burn leaves the row under `own` out
/// (`in_pool_without`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn projections_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    tier: &str,
    now: i64,
    read: BurnRead,
    own: Option<&PinKey>,
) -> BTreeMap<String, Projection> {
    // A usage-based seat holds no plan quota. The plan readings it kept from before it converted stand until their
    // reset, and a projection on them would fire a shedding round on a window the seat no longer draws.
    if snapshot.account(account_id).map(|a| a.kind) == Some(AccountKind::ApiKey)
        || snapshot.usage_based(account_id)
    {
        return BTreeMap::new();
    }
    let keep = in_pool_without(snapshot, pool, own);
    let average_ms = burn_average_ms(pool);
    let speed = match read {
        BurnRead::Achieved => 1.0,
        BurnRead::Demand => state.slow_down_speed(pool, account_id, now),
    };
    let burn = match read {
        BurnRead::Achieved => state.burn_shares(account_id, &keep, tier, now, average_ms),
        BurnRead::Demand => state.demand_shares(account_id, &keep, tier, now, average_ms, speed),
    };
    // The readings and horizons first, so the band's segments are cut over each window's own horizon. A window with
    // no burn projects its prediction.
    let mut projections = BTreeMap::new();
    let end = account_end(snapshot, account_id, now);
    for window in all_plan_windows() {
        let Some(reading) = state.predicted(account_id, window, now) else {
            continue;
        };
        let (horizon, bound) = horizon_of(window, &reading, end, now);
        // The limit is the one the horizon's bound sets, and the ceiling where the stretch tests the window.
        let limit = if projection_tested(pool, window) {
            bound.limit(pool, window)
        } else {
            ceiling(pool, window)
        };
        projections.insert(
            window.to_string(),
            Projection {
                predicted: reading.utilization,
                burn: burn.get(window).copied().unwrap_or(0.0),
                horizon,
                limit,
                at_horizon: reading.utilization,
                banded: Banded::default(),
                reset: reading.reset,
            },
        );
    }
    let banded = state.banded_shares(account_id, &keep, tier, now, average_ms, speed, &|window| {
        projections
            .get(window)
            .map(|projection: &Projection| projection.horizon)
    });
    for (window, banded) in banded {
        if let Some(projection) = projections.get_mut(&window) {
            projection.at_horizon = projection.predicted + banded.increase();
            projection.banded = banded;
        }
    }
    projections
}

/// A rate over a span as the band weighs it: pieces of `[from, to)` each at the rate the band gives it there, built
/// from a rate read at `now` (which carries the band of `now`, so the plain rate is it over the first segment's weight)
/// and added across pins of different regimes piece by piece.
// Public since `SlowDownRead` is and test literals build it outside `slow_down_of`. `pieces` stays private, so a
// `Banded` is built only through its own functions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Banded {
    /// `(from, to, rate)`: milliseconds and share per second, in order and contiguous.
    pieces: Vec<(i64, i64, f64)>,
}

impl Banded {
    /// One piece at weight 1 over `[from, to)`: the unbanded read, what a literal outside `slow_down_of` carries and
    /// what the pricer's default segments give every reader.
    pub fn flat(rate: f64, from: i64, to: i64) -> Banded {
        Banded {
            pieces: vec![(from, to, rate)],
        }
    }

    /// A rate read at the span's start, under the first segment's band.
    pub(crate) fn of(rate_at_now: f64, segments: &[BandSegment]) -> Banded {
        let read_weight = segments.first().map_or(1.0, |segment| segment.weight);
        Banded::read_at(rate_at_now, read_weight, segments)
    }

    /// A rate read under another instant's band, a booking's at `booked_at`: the plain rate is `rate / read_weight`.
    pub(crate) fn read_at(rate: f64, read_weight: f64, segments: &[BandSegment]) -> Banded {
        let plain = rate / read_weight;
        Banded {
            pieces: segments
                .iter()
                .map(|segment| (segment.from, segment.to, plain * segment.weight))
                .collect(),
        }
    }

    /// The union of both pieces' edges, the rates added where both cover a piece. Two empty spans at one instant add
    /// into one, so the rate survives for `time_to`'s continuation past an ended horizon.
    pub(crate) fn plus(&self, other: &Banded) -> Banded {
        if self.pieces.is_empty() {
            return other.clone();
        }
        if other.pieces.is_empty() {
            return self.clone();
        }
        let mut edges: Vec<i64> = self
            .pieces
            .iter()
            .chain(&other.pieces)
            .flat_map(|&(from, to, _)| [from, to])
            .collect();
        edges.sort_unstable();
        edges.dedup();
        if let [at] = edges[..] {
            let rate = |pieces: &[(i64, i64, f64)]| pieces.last().map_or(0.0, |piece| piece.2);
            return Banded {
                pieces: vec![(at, at, rate(&self.pieces) + rate(&other.pieces))],
            };
        }
        let rate_at = |pieces: &[(i64, i64, f64)], at: i64| {
            pieces
                .iter()
                .find(|&&(from, to, _)| from <= at && at < to)
                .map(|piece| piece.2)
        };
        Banded {
            pieces: edges
                .windows(2)
                .filter_map(|pair| {
                    let (from, to) = (pair[0], pair[1]);
                    match (rate_at(&self.pieces, from), rate_at(&other.pieces, from)) {
                        (None, None) => None,
                        (a, b) => Some((from, to, a.unwrap_or(0.0) + b.unwrap_or(0.0))),
                    }
                })
                .collect(),
        }
    }

    /// Every rate over `by`, the speed's division, as `shares` divides the pins' sum, so the two agree to the bit.
    pub(crate) fn divided(&self, by: f64) -> Banded {
        Banded {
            pieces: self
                .pieces
                .iter()
                .map(|&(from, to, rate)| (from, to, rate / by))
                .collect(),
        }
    }

    /// The share the rate adds over the span.
    pub(crate) fn increase(&self) -> f64 {
        self.pieces
            .iter()
            .map(|&(from, to, rate)| rate * seconds(to - from))
            .sum()
    }

    /// The share the rate adds from the span's start to `at`, nothing past the span's end.
    pub(crate) fn increase_until(&self, at: i64) -> f64 {
        self.pieces
            .iter()
            .filter(|&&(from, _, _)| from < at)
            .map(|&(from, to, rate)| rate * seconds(to.min(at) - from))
            .sum()
    }

    /// Milliseconds from the span's start until the accumulated increase reaches `room`, the last piece's rate
    /// continued past the span: `Some(0)` for a room at or under zero, `None` where no piece carries a rate.
    pub(crate) fn time_to(&self, room: f64) -> Option<i64> {
        if room <= 0.0 {
            return Some(0);
        }
        let start = self.pieces.first()?.0;
        let (last, walked) = self.pieces.split_last()?;
        let mut left = room;
        for &(from, to, rate) in walked {
            let adds = rate * seconds(to - from);
            if rate > 0.0 && adds >= left {
                return Some(((seconds(from - start) + left / rate) * 1_000.0).round() as i64);
            }
            left -= adds;
        }
        // The last piece is solved linearly, inside the span or past it.
        let &(from, _, rate) = last;
        (rate > 0.0).then(|| ((seconds(from - start) + left / rate) * 1_000.0).round() as i64)
    }
}

/// The projection of `account_id` on `window` at its horizon on the burn `read` names: `None` for an account with no
/// reading of the window, an api_key account or a usage-based seat.
#[allow(clippy::too_many_arguments)]
pub(crate) fn projection_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    tier: &str,
    window: &str,
    now: i64,
    read: BurnRead,
) -> Option<Projection> {
    projections_of(state, snapshot, pool, account_id, tier, now, read, None).remove(window)
}

/// One account's slow-down read on the 5-hour window at an instant: the figures the stretch and the factor are read
/// from, the two of them, and the speed the demand was divided by.
#[derive(Clone, Debug, PartialEq)]
pub struct SlowDownRead {
    /// The prediction on the window.
    pub utilization: f64,
    pub horizon: i64,
    /// The 5-hour reading's reset, `None` where the reading names none.
    pub reset: Option<i64>,
    /// Share of the window per second.
    pub demand: f64,
    /// The demand over `[now, horizon)` as the band weighs it, which the stretch and the factor read.
    pub banded: Banded,
    /// The time between the instant the unslowed demand passes the ceiling and the horizon, 0 when it never does.
    pub stretch_ms: i64,
    /// The pool's `slow_down_stretch_seconds`.
    pub threshold_ms: i64,
    pub factor: f64,
    /// The share of the read's slow-down the holds apply, by the ramp's one rule (`slow_down_ramp`): the time since the
    /// account's span opened over one burn average, clamped to [0, 1], 1 on a window at or past the ceiling and 0 at a
    /// factor of 1.
    pub ramp: f64,
    /// The factor the plan received, `1 - (1 - factor) x ramp`: the hold's outcome names it.
    pub ramped: f64,
    pub speed: f64,
    /// The holds of the pool's pins on the account whose pace has a cycle, every kept pin whose agent has not ended
    /// weighted by its priced decayed rate on `five_hour`: a pin without a cycle runs unheld and a pin whose cycle is
    /// too long for the cap is held at the cap, and the rest share the remaining reduction at one speed, so the
    /// account's pins realize the ramped factor together as far as the cap lets them. Empty while no live pin on the
    /// account has a cycle.
    pub plan: Vec<PinHold>,
    /// The speed the account's pins realize under the plan, `sum(b x s) / sum(b)` over the pins `plan` weighs, with
    /// `s = c / (c + h)` and `s = 1` for a pin without a cycle, so 1 while the plan is empty: the one figure the
    /// selection folds into the entry's speed. The demand divides every kept pin's burn by it, so an unheld pin's burn
    /// reads at the pace it runs. Positive, so the demand never divides by zero.
    pub realized: f64,
}

/// One pin's part of the account's plan: the cycle it was sized from and its hold, capped.
#[derive(Clone, Debug, PartialEq)]
pub struct PinHold {
    pub key: PinKey,
    pub cycle_ms: i64,
    pub hold_ms: i64,
}

/// The account's slow-down read at `now` on `pool`: `U` the 5-hour prediction, `R` the time to the 5-hour horizon, `B`
/// the 5-hour demand, the stretch `S = R - (ceiling - U) / B` and the factor `(ceiling - U) / (R x B)` clamped to
/// [0, 1], which is `1 - S / R`, with `(ceiling - U) / B` the time the demand takes to fill the room and `R x B` what
/// it adds by the horizon, both read over the band's segments. A demand of nothing below the ceiling reads no stretch
/// and a factor of 1, and a window at or past the ceiling a stretch of at least `R` and a factor of 0, whatever the
/// demand. A horizon already reached reads a factor of 0 at or past the ceiling and 1 under it, so no `0 / 0` reaches
/// the entry. `None` for a pool whose slow-down is off, an api_key account (no 5-hour window), an account with no
/// 5-hour reading, and one whose tier the profile has not named or whose plan the cost model does not price, which
/// prices no demand.
pub(crate) fn slow_down_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    tier: &str,
    now: i64,
) -> Option<SlowDownRead> {
    if !slow_down(pool)
        || snapshot.account(account_id).map(|a| a.kind) == Some(AccountKind::ApiKey)
        || snapshot.priced_tier(account_id).is_none()
    {
        return None;
    }
    let reading = state.predicted(account_id, FIVE_HOUR, now)?;
    let utilization = reading.utilization;
    let (horizon, _) = horizon_of(
        FIVE_HOUR,
        &reading,
        account_end(snapshot, account_id, now),
        now,
    );
    let speed = state.slow_down_speed(pool, account_id, now);
    let demand = state
        .demand_shares(
            account_id,
            &in_pool(snapshot, pool),
            tier,
            now,
            burn_average_ms(pool),
            speed,
        )
        .get(FIVE_HOUR)
        .copied()
        .unwrap_or(0.0);
    let banded = state
        .banded_shares(
            account_id,
            &in_pool(snapshot, pool),
            tier,
            now,
            burn_average_ms(pool),
            speed,
            &|window| (window == FIVE_HOUR).then_some(horizon),
        )
        .remove(FIVE_HOUR)
        .unwrap_or_default();
    let limit = ceiling(pool, FIVE_HOUR);
    let to_horizon = seconds(horizon - now);
    let at_ceiling = utilization >= limit;
    // The stretch in whole milliseconds, taken from the reach `time_to` already rounded, so the figure is rounded once
    // and agrees with the reach to the millisecond.
    let (stretch_ms, factor) = if at_ceiling {
        let overrun = if demand > 0.0 {
            (utilization - limit) / demand
        } else {
            0.0
        };
        (((to_horizon + overrun) * 1_000.0).round() as i64, 0.0)
    } else if demand <= 0.0 || to_horizon <= 0.0 {
        (0, 1.0)
    } else {
        let room = limit - utilization;
        (
            banded
                .time_to(room)
                .map_or(0, |reached| (horizon - now - reached).max(0)),
            (room / banded.increase()).clamp(0.0, 1.0),
        )
    };
    let average_ms = burn_average_ms(pool);
    let ramp = slow_down_ramp(state, pool, account_id, factor, now);
    let ramped = 1.0 - (1.0 - factor) * ramp;
    // Every kept pin, each weighted by its own priced burn on the window: a pin without a cycle is never held, but its
    // burn runs at full speed and weighs in the speed the demand divides by.
    let pins: Vec<(PinKey, Option<f64>, f64)> = state
        .pins_on(account_id, &in_pool(snapshot, pool))
        .into_iter()
        .map(|p| {
            let cycle = state
                .pin_pace(&p.key())
                .and_then(|pace| pace.cycle_ms)
                .filter(|cycle| *cycle > 0.0);
            let weight = state
                .share()
                .rate(
                    FIVE_HOUR,
                    p.entrypoint.as_deref(),
                    tier,
                    &p.model,
                    &p.burn,
                    now,
                )
                .map_or(0.0, |rate| burn_decayed(rate, p.burn_at, now, average_ms));
            (p.key(), cycle, weight)
        })
        .collect();
    let (plan, realized) = water_fill(&pins, ramped, slow_down_hold_ms(pool));
    Some(SlowDownRead {
        utilization,
        horizon,
        reset: reading.reset,
        demand,
        banded,
        stretch_ms,
        threshold_ms: slow_down_stretch_ms(pool),
        factor,
        ramp,
        ramped,
        speed,
        plan,
        realized,
    })
}

/// The share of the slow-down's factor the holds apply at `now`, the ramp's one rule for the selection and the
/// account's projection: 1 while the account's 5-hour prediction is at or past the pool's ceiling, a measured fact and
/// not a projection a burst could retract, 0 at a factor of 1, which holds nothing, and between them the share of one
/// pool burn average that has passed since the span of the account's entry in force opened, clamped to [0, 1]. A burst
/// of a few minutes whose demand projects past the ceiling so costs holds in proportion to how long it has stood, and a
/// demand that lasts gets the full factor once the average has absorbed it. The ramp lives and ends with the span: a
/// return to 1 shorter than `SLOW_DOWN_WAIT_MS` keeps the span's start and so continues the ramp, a return that lasts
/// the wait closes both, and the entry of a passed window is not in force, so the ramp restarts with every window.
pub(crate) fn slow_down_ramp(
    state: &RoutingState,
    pool: &PoolRow,
    account_id: &str,
    factor: f64,
    now: i64,
) -> f64 {
    let at_ceiling = state
        .predicted(account_id, FIVE_HOUR, now)
        .is_some_and(|reading| reading.utilization >= ceiling(pool, FIVE_HOUR));
    if at_ceiling {
        return 1.0;
    }
    if factor >= 1.0 {
        return 0.0;
    }
    // A span whose factor has read 1 for the wait has closed, though the drain that closes it may not have run yet.
    let stood = state
        .slow_down_in_force(pool, account_id, now)
        .filter(|entry| entry.closed_at(now).is_none())
        .and_then(|entry| entry.slowed_since)
        .map_or(0, |since| (now - since).max(0));
    (stood as f64 / burn_average_ms(pool).max(1) as f64).clamp(0.0, 1.0)
}

/// Whether the slow-down may hold this request on a pin whose account is of `kind`: a main or subagent turn, or one
/// with no class header, portable, and on an account with a 5-hour window. An auxiliary request, a compaction, a fast
/// request, a probe and a token count are never held, and their burn counts.
pub(crate) fn slowable(request: &SelectRequest, kind: Option<AccountKind>) -> bool {
    let held_class = request
        .request_class
        .as_deref()
        .is_none_or(|class| class == "main" || class == "subagent");
    held_class
        && request.portable
        && !(request.fast || request.probe || request.count)
        && kind != Some(AccountKind::ApiKey)
}

/// A fit's word, from best to worst: the order the ladder lands a fresh session by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FitWord {
    Fits,
    RewriteOnly,
    LeastBad,
    Ending,
    Spent,
}

impl FitWord {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FitWord::Fits => "fits",
            FitWord::RewriteOnly => "rewrite_only",
            FitWord::LeastBad => "least_bad",
            FitWord::Ending => "ending",
            FitWord::Spent => "spent",
        }
    }
}

/// What the gate read for one candidate: the verdict and the figures the `fit` step and the candidate table state.
pub(crate) struct FitRead {
    /// `Ending`, `Fits`, `LeastBad` or `RewriteOnly`.
    pub verdict: FitWord,
    /// The runway on the binding window: `None` for an unbounded one (no burn on it, or a runway past its window's
    /// reset on every window), `Some(0)` when the rewrite alone reaches the ceiling.
    pub runway_ms: Option<i64>,
    pub binding: Option<String>,
    /// The runway on the binding window over every bounded window, one past its reset included: what `longest` and
    /// `runway_ties` rank by, so a window past its reset still orders the placement, while `runway_ms` and `binding`
    /// show only the windows whose runway ends at or before their reset.
    pub rank_runway_ms: Option<i64>,
    /// The runway the third rung ranks a `least_bad` member by: per window zero where the prediction with the
    /// rewrite stands at or past 1.0, else the banded time to 1.0, and for the member the pick of `binding_of` over
    /// its bounded windows, one past its reset included. Zero for a paid source whose full bucket refuses the request
    /// at this instant, which names no window. None for a member no window bounds.
    pub wall_runway_ms: Option<i64>,
    /// The runway to 1.0 and its window as a landing's `least_bad` row states them: read over the windows whose runway
    /// to 1.0 ends at or before their reset, as `runway_ms` and `binding` read the ceiling's.
    pub wall_shown_ms: Option<i64>,
    pub wall_binding: Option<String>,
    pub projection: BTreeMap<String, f64>,
    pub horizon: BTreeMap<String, i64>,
    pub rewrite: BTreeMap<String, f64>,
    pub reserved: BTreeMap<String, f64>,
    pub end: Option<i64>,
    /// The word of each window the request's model draws that the account holds a reading of, one its set no longer
    /// holds included, by three tests in order: `spent` at a prediction of 1.0 or more, `ending` inside the account's
    /// wind-down, else `fits`, `rewrite_only` or `least_bad` on the window's own runway. A window outside the set has
    /// no runway read, so the third test reads `fits` there. A paid source and, on a placement, an untiered account
    /// give each window the read's verdict. Only `fresh_fit`'s read, which the pools route answers from, fills it.
    pub words: BTreeMap<String, FitWord>,
}

impl FitRead {
    /// The runway the ladder ranks a member on `rung` by: `rank_runway_ms` on `Fits` and `RewriteOnly`,
    /// `wall_runway_ms` on `LeastBad`, none on a rung that ranks by no runway.
    pub(crate) fn rung_runway_ms(&self, rung: FitWord) -> Option<i64> {
        match rung {
            FitWord::Fits | FitWord::RewriteOnly => self.rank_runway_ms,
            FitWord::LeastBad => self.wall_runway_ms,
            FitWord::Ending | FitWord::Spent => None,
        }
    }
}

/// How `fit_of` reads a window with no reading and an account with no tier. `Move` reads either gap as least bad: a
/// move pays a cold rewrite on its destination, so it trusts only one it can price on every window the model draws that
/// the destination's set holds. `Placement` skips a window with no reading and reads an untiered account as fitting
/// with an unbounded runway and no binding window, since a fresh session counts an unsampled account as untouched and
/// nothing prices an untiered account's rewrite or burn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Judge {
    Move,
    Placement,
}

/// The fit of `pin` (or a fresh session) on `candidate`: the runway per window the request's model draws that the
/// candidate's set holds (a move's runway read to `SHED_FIT_BAND`'s line under the ceiling), the predicate of the three
/// verdicts and the end exclusion, with the unknowns read as `judge` says. `planned` holds the reservations a shedding
/// round planned before this read, summed into the candidate's prediction and burn as the live ones are, and is empty
/// for a move rule, the partition and rung 4. `own` is the request's pin key on a placement's own read, whose row the
/// account's burn leaves out since the placement replaces it, and `None` for a move.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fit_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    figures: Figures<'_>,
    candidate: &str,
    planned: &[Reservation],
    now: i64,
    judge: Judge,
    own: Option<&PinKey>,
) -> FitRead {
    fit_read(
        state, snapshot, pool, request, figures, candidate, planned, now, judge, own, false,
    )
}

/// `fit_of`'s read, with each window's word in `words` where `words` is set.
#[allow(clippy::too_many_arguments)]
fn fit_read(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    figures: Figures<'_>,
    candidate: &str,
    planned: &[Reservation],
    now: i64,
    judge: Judge,
    own: Option<&PinKey>,
    words: bool,
) -> FitRead {
    let mut read = FitRead {
        verdict: FitWord::LeastBad,
        runway_ms: None,
        binding: None,
        rank_runway_ms: None,
        wall_runway_ms: None,
        wall_shown_ms: None,
        wall_binding: None,
        projection: BTreeMap::new(),
        horizon: BTreeMap::new(),
        rewrite: BTreeMap::new(),
        reserved: BTreeMap::new(),
        end: None,
        words: BTreeMap::new(),
    };
    // Each answer words every window the model draws that the account holds a reading of, one its set no longer holds
    // included, since `holds_allowance` reads that reading too and the word must not promise allowance it denies.
    let held = || {
        plan_windows(&request.canonical_model)
            .filter(move |window| state.predicted(candidate, window, now).is_some())
    };
    let spent_or = |window: &str, word: FitWord| {
        if spent_reading(state, candidate, window, now).is_some() {
            FitWord::Spent
        } else {
            word
        }
    };
    let every = |word: FitWord| -> BTreeMap<String, FitWord> {
        held().map(|window| (window.to_string(), word)).collect()
    };
    // An account inside the wind-down is no destination: the operator's `wind_down_minutes` is the one rule before
    // the end, and the same test keeps a fresh session off the account and drains its cold pins.
    if winding_down(snapshot, pool, candidate, now) {
        read.end = account_end(snapshot, candidate, now);
        read.verdict = FitWord::Ending;
        // A spent window reads `spent` ahead of the wind-down, so the member ranks behind the ones that only end.
        if words {
            read.words = held()
                .map(|window| (window.to_string(), spent_or(window, FitWord::Ending)))
                .collect();
        }
        return read;
    }
    if paid_source(snapshot, candidate) {
        // A paid source holds no plan window to project: an api_key bucket refills within its minute, so a line under
        // 1.0 says the account has room, and a usage-based seat reads no bucket at all.
        let room = state
            .sample(candidate)
            .and_then(|s| s.worst_bucket(now))
            .is_none_or(|r| r.utilization < 1.0);
        read.verdict = if room {
            FitWord::Fits
        } else {
            FitWord::LeastBad
        };
        // The full bucket refuses the request at this instant, so the third rung ranks the source behind any room.
        if !room {
            read.wall_runway_ms = Some(0);
            read.wall_shown_ms = Some(0);
        }
        if words {
            read.words = every(read.verdict);
        }
        return read;
    }
    // An unnamed tier, or a plan the cost model does not price, prices neither the rewrite nor the burn, so for a move
    // the figures are the prediction alone and the verdict stays least bad.
    let tier = snapshot.priced_tier(candidate);
    if tier.is_none() && judge == Judge::Placement {
        read.verdict = FitWord::Fits;
        if words {
            read.words = every(read.verdict);
        }
        return read;
    }
    let (rewrite, pin_burn, pin_regime) = tier
        .map(|tier| pin_figures(state, pool, candidate, request, figures, tier, now))
        .unwrap_or_default();
    let share = state.share();
    let projections = projections_of(
        state,
        snapshot,
        pool,
        candidate,
        tier.unwrap_or_default(),
        now,
        BurnRead::Demand,
        own,
    );
    let planned: Vec<&Reservation> = planned
        .iter()
        .filter(|r| r.account_id == candidate)
        .collect();
    let mut unread = false;
    let mut every_fits = true;
    let mut rewrite_lands = true;
    // Every bounded runway with whether it falls short of its horizon: the window the pin reaches first among those
    // it overruns binds, else the first it reaches at all, so a zero runway on any window is the candidate's runway.
    // `shown` leaves out a window whose runway passes its reset, which the displayed pair reads as unbounded as it
    // reads a runway no burn bounds, while the verdict and the rank read every bounded window.
    let mut bounded: Vec<(&str, i64, bool)> = Vec::new();
    let mut shown: Vec<(&str, i64, bool)> = Vec::new();
    // The same two lists for the runway to 1.0, which the third rung ranks a `least_bad` member by.
    let mut walls: Vec<(&str, i64, bool)> = Vec::new();
    let mut walls_shown: Vec<(&str, i64, bool)> = Vec::new();
    for window in state.plan_windows_held(candidate, &request.canonical_model) {
        let Some(projection) = projections.get(window) else {
            if judge == Judge::Move {
                unread = true;
            }
            continue;
        };
        let planned_rewrite: f64 = planned.iter().filter_map(|r| r.rewrite.get(window)).sum();
        let segments = |entrypoint: Option<&str>| {
            share.band_segments(window, entrypoint, now, projection.horizon)
        };
        // Each planned booking weighs its own regime's band from the plain rate it was priced at on `booked_at`, so a
        // round that plans bookings of two regimes reads each as the meter will charge it.
        let planned_burn = planned
            .iter()
            .filter_map(|r| {
                let burn = *r.burn.get(window)?;
                let entrypoint = r.entrypoint.as_deref();
                let read_weight = share.band_weight_at(window, entrypoint, r.booked_at);
                Some(Banded::read_at(burn, read_weight, &segments(entrypoint)))
            })
            .fold(Banded::default(), |sum, booking| sum.plus(&booking));
        let predicted = projection.predicted + planned_rewrite;
        // The projection is the account's alone, and the runway adds the pin's own burn to it.
        let account_burn = projection.banded.plus(&planned_burn);
        let rewrite = rewrite.get(window).copied().unwrap_or(0.0);
        let burn = match pin_burn.get(window) {
            Some(rate) => account_burn.plus(&Banded::of(*rate, &segments(pin_regime))),
            None => account_burn.clone(),
        };
        let limit = ceiling(pool, window);
        let span = projection.horizon - now;
        // A destination whose room rests on the moved pin's own burn estimate fills at its next under-read, and the
        // next round sheds the pin back, so a move's runway reads to a line the band holds under the ceiling. The
        // rewrite's test and a fresh placement read the ceiling.
        let line = match (judge, pin_burn.get(window)) {
            (Judge::Move, Some(rate)) => limit - SHED_FIT_BAND * rate * seconds(span),
            _ => limit,
        };
        read.projection
            .insert(window.to_string(), predicted + account_burn.increase());
        read.horizon.insert(window.to_string(), projection.horizon);
        read.rewrite.insert(window.to_string(), rewrite);
        read.reserved.insert(
            window.to_string(),
            state.reserved(candidate, window) + planned_rewrite,
        );
        // The verdict reads the rewrite against the ceiling itself, and the rounding touches the figure alone.
        let lands = predicted + rewrite < limit;
        if !lands {
            rewrite_lands = false;
        }
        let push = |all: &mut Vec<(&'static str, i64, bool)>,
                    shown: &mut Vec<(&'static str, i64, bool)>,
                    ms: i64| {
            let short = ms < span;
            all.push((window, ms, short));
            if !passes_reset(ms, projection.reset, now) {
                shown.push((window, ms, short));
            }
            short
        };
        // `time_to` answers zero where the prediction with the rewrite already stands at or past 1.0.
        if let Some(wall_ms) = burn.time_to(1.0 - predicted - rewrite) {
            push(&mut walls, &mut walls_shown, wall_ms);
        }
        let runway_ms = if !lands {
            Some(0)
        } else {
            burn.time_to(line - predicted - rewrite)
        };
        if words {
            let word = if !lands {
                FitWord::LeastBad
            } else if runway_ms.is_some_and(|runway_ms| runway_ms < span) {
                FitWord::RewriteOnly
            } else {
                FitWord::Fits
            };
            read.words
                .insert(window.to_string(), spent_or(window, word));
        }
        let Some(runway_ms) = runway_ms else {
            continue;
        };
        if push(&mut bounded, &mut shown, runway_ms) {
            every_fits = false;
        }
    }
    // The loop walks the set's windows alone, so the verdict and the runways never read a window outside it.
    if words {
        for window in held() {
            read.words
                .entry(window.to_string())
                .or_insert_with(|| spent_or(window, FitWord::Fits));
        }
    }
    let readout = |all: &[(&'static str, i64, bool)], shown: &[(&'static str, i64, bool)]| {
        (binding_of(all).map(|(_, ms)| ms), binding_of(shown))
    };
    let (rank_runway_ms, binding) = readout(&bounded, &shown);
    read.rank_runway_ms = rank_runway_ms;
    if let Some((window, runway_ms)) = binding {
        read.binding = Some(window.to_string());
        read.runway_ms = Some(runway_ms);
    }
    let (wall_runway_ms, wall_binding) = readout(&walls, &walls_shown);
    read.wall_runway_ms = wall_runway_ms;
    if let Some((window, wall_ms)) = wall_binding {
        read.wall_binding = Some(window.to_string());
        read.wall_shown_ms = Some(wall_ms);
    }
    read.verdict = if tier.is_none() || unread {
        FitWord::LeastBad
    } else if every_fits {
        FitWord::Fits
    } else if rewrite_lands {
        FitWord::RewriteOnly
    } else {
        FitWord::LeastBad
    };
    read
}

/// The fit a fresh placement reads on `candidate` at `now` for a request on `model` that prices no rewrite, over the
/// pool's fresh prior `prior` (`fresh_prior`'s rows, read once for the pool): the entry of one regime in the list of
/// fits that the pools route answers for a member. The request carries no pin, no body, no failover entry, no
/// classifier, probe, count or fast mark and no candidates, so the read is the gate's own for a fresh session on the
/// windows `model` draws. The prior prices under the regime that `entrypoint` names, the base one for `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fresh_fit(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    prior: &[PinRow],
    candidate: &str,
    model: &str,
    entrypoint: Option<&str>,
    now: i64,
) -> FitRead {
    // Every member is written out, as `standing_request` writes them, so a member the type gains states its value here.
    let request = SelectRequest {
        request_id: String::new(),
        attempt: 0,
        seq: 0,
        key_id: String::new(),
        key_generation: 0,
        agent_id: String::new(),
        session_id: String::new(),
        canonical_model: model.to_string(),
        folded_model: model.to_string(),
        candidates: Vec::new(),
        portable: false,
        credit_account: None,
        attempted: Vec::new(),
        now,
        received_at: now,
        fallback_ms: 0,
        revision: 0,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        boundaries: Arc::from([]),
        client: ClientClass::Other,
        entrypoint: entrypoint.map(String::from),
        static_boundaries: Arc::from([]),
        marker: false,
        parent_agent_id: None,
        holds: Vec::new(),
        failed_over: None,
        pin_ended: false,
        dialed: false,
        held_out: false,
        lane: String::new(),
        fast: false,
        classifier: false,
        body_tokens: 0,
        effort: None,
    };
    fit_read(
        state,
        snapshot,
        pool,
        &request,
        Figures::Fresh(prior),
        candidate,
        &[],
        now,
        Judge::Placement,
        None,
        true,
    )
}

/// The binding window among `runways` with its runway: the shortest of those short of their horizon, else the
/// shortest of all, one choice for the rank and the displayed pair alike.
fn binding_of<'w>(runways: &[(&'w str, i64, bool)]) -> Option<(&'w str, i64)> {
    runways
        .iter()
        .filter(|(_, _, short)| *short)
        .min_by_key(|(_, runway_ms, _)| *runway_ms)
        .or_else(|| runways.iter().min_by_key(|(_, runway_ms, _)| *runway_ms))
        .map(|&(window, runway_ms, _)| (window, runway_ms))
}

/// The record's key for a per-window fact of the fit table, the shed line or the reserve step, spelled once per plan
/// window since a fact's key is static. `None` for a window the table does not name.
fn fit_key(kind: &str, window: &str) -> Option<&'static str> {
    Some(match (kind, window) {
        ("horizon", FIVE_HOUR) => "horizon_five_hour",
        ("horizon", SEVEN_DAY) => "horizon_seven_day",
        ("horizon", SEVEN_DAY_FABLE) => "horizon_seven_day_fable",
        ("wall", FIVE_HOUR) => "wall_five_hour",
        ("wall", SEVEN_DAY) => "wall_seven_day",
        ("wall", SEVEN_DAY_FABLE) => "wall_seven_day_fable",
        ("projection", FIVE_HOUR) => "projection_five_hour",
        ("projection", SEVEN_DAY) => "projection_seven_day",
        ("projection", SEVEN_DAY_FABLE) => "projection_seven_day_fable",
        ("rewrite", FIVE_HOUR) => "rewrite_five_hour",
        ("rewrite", SEVEN_DAY) => "rewrite_seven_day",
        ("rewrite", SEVEN_DAY_FABLE) => "rewrite_seven_day_fable",
        ("reserved", FIVE_HOUR) => "reserved_five_hour",
        ("reserved", SEVEN_DAY) => "reserved_seven_day",
        ("reserved", SEVEN_DAY_FABLE) => "reserved_seven_day_fable",
        ("burn", FIVE_HOUR) => "burn_five_hour",
        ("burn", SEVEN_DAY) => "burn_seven_day",
        ("burn", SEVEN_DAY_FABLE) => "burn_seven_day_fable",
        ("over_since", FIVE_HOUR) => "over_since_five_hour",
        ("over_since", SEVEN_DAY) => "over_since_seven_day",
        ("over_since", SEVEN_DAY_FABLE) => "over_since_seven_day_fable",
        ("wait", FIVE_HOUR) => "wait_five_hour",
        ("wait", SEVEN_DAY) => "wait_seven_day",
        ("wait", SEVEN_DAY_FABLE) => "wait_seven_day_fable",
        _ => return None,
    })
}

/// The `fit` walk step over the candidates read, its verdict the order taken (`fits`, `rewrite_only`, `least_bad`)
/// or `none` when no order holds a candidate. One row per candidate, each fact about its account, with `runway_ms`
/// absent for an unbounded runway: one no burn bounds, or one past its window's reset on every window.
pub(crate) fn fit_step(reads: &[(String, FitRead)], verdict: &'static str) -> WalkStep {
    let mut facts = Vec::new();
    for (account_id, read) in reads {
        let mut row = Vec::new();
        for (window, projection) in &read.projection {
            if let Some(key) = fit_key("projection", window) {
                row.push(Fact::number(key, *projection).unit("percent"));
            }
        }
        if let Some(runway_ms) = read.runway_ms {
            row.push(Fact::int("runway_ms", runway_ms).unit("ms"));
        }
        for (window, rewrite) in &read.rewrite {
            if let Some(key) = fit_key("rewrite", window) {
                row.push(Fact::number(key, *rewrite).unit("percent"));
            }
        }
        for (window, reserved) in &read.reserved {
            if let Some(key) = fit_key("reserved", window) {
                row.push(Fact::number(key, *reserved).unit("percent"));
            }
        }
        if let Some(binding) = &read.binding {
            row.push(Fact::text("binding", binding.clone()));
        }
        if let Some(end) = read.end {
            row.push(Fact::int("end", end).unit("time"));
        }
        row.push(Fact::text("verdict", read.verdict.as_str()));
        facts.extend(row.into_iter().map(|fact| fact.about(account_id)));
    }
    WalkStep {
        step: "fit",
        verdict,
        facts,
    }
}

/// The `reserve` step of a booking and the outcome's copy of it: the account, the rewrite's share per plan window,
/// the burn per hour, the expiry and the wire attempt that booked it, every fact about the account.
fn reserve_step(reservation: &Reservation) -> (WalkStep, ReservationFacts) {
    let account_id = reservation.account_id.as_str();
    let burn: BTreeMap<String, f64> = reservation
        .burn
        .iter()
        .map(|(window, share)| (window.clone(), share * 3_600.0))
        .collect();
    let mut facts = vec![account_fact(account_id)];
    for (window, share) in &reservation.rewrite {
        if let Some(key) = fit_key("reserved", window) {
            facts.push(Fact::number(key, *share).unit("percent").about(account_id));
        }
    }
    for (window, share) in &burn {
        if let Some(key) = fit_key("burn", window) {
            facts.push(
                Fact::number(key, *share)
                    .unit("percent_per_hour")
                    .about(account_id),
            );
        }
    }
    facts.push(
        Fact::int("expires_at", reservation.expires_at)
            .unit("time")
            .about(account_id),
    );
    facts.push(
        Fact::count("attempt", u64::from(reservation.attempt))
            .unit("count")
            .about(account_id),
    );
    (
        WalkStep {
            step: "reserve",
            verdict: "booked",
            facts,
        },
        ReservationFacts {
            account_id: reservation.account_id.clone(),
            rewrite: reservation.rewrite.clone(),
            burn,
            expires_at: reservation.expires_at,
        },
    )
}

pub(crate) fn account_end(snapshot: &RegistrySnapshot, account_id: &str, now: i64) -> Option<i64> {
    snapshot
        .term(account_id)
        .and_then(|term| term::end_of(term, now))
}

/// The share of `window` that has elapsed at `at`, from the window's start (its reset `reset_at` less the window's
/// length, `window_length_ms`'s five hours for the 5-hour window and a week for any other name) to the earlier of the
/// reset and the account's `end`, clamped to the range 0 to 1. An end at or past the reset leaves the share to the
/// reset, an end at or before the window's start reads 1, and without a reset the share is 0. The one arithmetic behind
/// the engine's pace reading and the ledger's lead over a raw sample, so the two readings cannot drift apart.
pub fn elapsed_share_at(window: &str, reset_at: Option<i64>, end: Option<i64>, at: i64) -> f64 {
    let length = window_length_ms(window);
    reset_at
        .zip(window_deadline(reset_at, end))
        .map_or(0.0, |(reset, deadline)| {
            let start = reset.saturating_sub(length);
            let span = deadline.saturating_sub(start);
            if span <= 0 {
                return 1.0;
            }
            (at.saturating_sub(start) as f64 / span as f64).clamp(0.0, 1.0)
        })
}

/// The instant a window's elapsed share runs toward: the earlier of the window's reset and the account's end,
/// `None` without a reset.
pub fn window_deadline(reset: Option<i64>, account_end: Option<i64>) -> Option<i64> {
    reset.map(|reset| account_end.map_or(reset, |end| end.min(reset)))
}

/// Whether the account's allowance is known: a paid source (an api_key account or a usage-based seat) has no plan
/// window to read, and a subscription account holds a prediction of every window `holds_allowance` reads for the
/// request that the account has: the model's own week and each shared window of the account's set for a model with
/// a week of its own, each shared window of the set otherwise.
fn allowance_known(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &str,
    now: i64,
) -> bool {
    let known = |name: &str| state.predicted(account_id, name, now).is_some();
    // A shared window the account's newest clean poll lacked has no reading to wait for.
    let shared = |name: &str| !state.has_window(account_id, name) || known(name);
    paid_source(snapshot, account_id)
        || ((window == SEVEN_DAY || known(window)) && shared(FIVE_HOUR) && shared(SEVEN_DAY))
}

/// What step 4's rules answered: the pin stays, moves to one of a set under a reason with the fit reads the strategy's
/// tie is broken by, or waits in place until its spent window has reset and its account's cooldown has ended. The
/// reset, the window and the wait are the spent line's facts, written where the rule reads them.
enum MoveVerdict<'a> {
    Stay,
    Move(MigrateReason, Vec<&'a Candidate>, Vec<(String, FitRead)>),
    Wait { until: i64 },
}

/// What a rule's gate answered: the destination set of the first non-empty order, the fit reads whose runway breaks
/// the strategy's tie, the `fit` step over every candidate read, and the candidates the end exclusion dropped. The
/// reads are the gate's when the set is the `fits` order and empty otherwise, so a `rewrite_only` or `least_bad` set
/// is drawn as the strategy alone says. `rank` finds a candidate's read by its account id, so the set the cap filter
/// narrows still reads its runway.
struct Gate<'a> {
    set: Vec<&'a Candidate>,
    reads: Vec<(String, FitRead)>,
    step: WalkStep,
    ending: Vec<String>,
}

/// The candidates of `set` the gate read as `verdict`, each with its read.
fn order<'a, 'r>(
    set: &[&'a Candidate],
    reads: &'r [(String, FitRead)],
    verdict: FitWord,
) -> Vec<(&'a Candidate, &'r FitRead)> {
    set.iter()
        .zip(reads)
        .filter(|(_, (_, read))| read.verdict == verdict)
        .map(|(c, (_, read))| (*c, read))
        .collect()
}

/// The candidates with the longest runway as `runway` reads it off each fit, an unbounded one longest of all, ties
/// kept for the pick.
fn longest<'a, 'r>(
    candidates: Vec<(&'a Candidate, &'r FitRead)>,
    runway: impl Fn(&FitRead) -> Option<i64>,
) -> Vec<(&'a Candidate, &'r FitRead)> {
    let key = |read: &FitRead| runway(read).unwrap_or(i64::MAX);
    let Some(best) = candidates.iter().map(|(_, read)| key(read)).max() else {
        return candidates;
    };
    candidates
        .into_iter()
        .filter(|(_, read)| key(read) == best)
        .collect()
}

/// The three orders over the siblings `keep` admits: the candidates the pin fits to the horizon on every window its
/// model draws that the candidate's set holds, else those where the rewrite alone fits at the longest runway, every tie
/// kept, else the rest. The `fits` order goes to the strategy whole: the strategy ranks it, and the longest runway on
/// the binding window breaks the strategy's tie. A tie inside the other two orders is the strategy's alone to break:
/// the fit has read every window the model draws that the candidate's set holds, so no second reading of a week orders
/// the candidates again. A candidate at its in-flight cap leaves the first two orders, so a move does not pick a full
/// account while a free fitting one exists, and reaches the last, where the cap filter reads it as today. The shed rule
/// (`fits_only`) takes the first order alone and answers none when nothing fits. Every read lands on the member's row
/// of the record too.
#[allow(clippy::too_many_arguments)]
fn gate<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    current: &PinRow,
    eligible: &[&'a Candidate],
    window: &str,
    keep: &dyn Fn(&Candidate) -> bool,
    fits_only: bool,
    rec: &mut Recorder,
) -> Gate<'a> {
    let now = request.now;
    let set: Vec<&'a Candidate> = eligible
        .iter()
        .copied()
        .filter(|c| c.account_id != current.account_id && keep(c))
        // An account with no allowance left in the model's own window is no destination: step 6 drops it, so
        // releasing the pin for a set of them would strand the request on nothing.
        .filter(|c| {
            window == SEVEN_DAY || holds_allowance(state, snapshot, &c.account_id, window, now)
        })
        .collect();
    let reads: Vec<(String, FitRead)> = set
        .iter()
        .map(|c| {
            (
                c.account_id.clone(),
                fit_of(
                    state,
                    snapshot,
                    pool,
                    request,
                    Figures::Pin(current),
                    &c.account_id,
                    &[],
                    now,
                    Judge::Move,
                    None,
                ),
            )
        })
        .collect();
    for (account_id, read) in &reads {
        if let Some(member) = rec.candidate_mut(account_id) {
            member.runway_ms = read.runway_ms;
            member.fit = Some(read.verdict.as_str());
            member.binding = read.binding.clone();
        }
    }
    let free = |(c, _): &(&Candidate, &FitRead)| under_cap(state, pool, &c.account_id);
    let fits: Vec<_> = order(&set, &reads, FitWord::Fits)
        .into_iter()
        .filter(free)
        .collect();
    let rewrite_only: Vec<_> = order(&set, &reads, FitWord::RewriteOnly)
        .into_iter()
        .filter(free)
        .collect();
    let least_bad: Vec<&'a Candidate> = set
        .iter()
        .copied()
        .zip(&reads)
        .filter(|(_, (_, read))| read.verdict != FitWord::Ending)
        .map(|(c, _)| c)
        .collect();
    let chosen = |candidates: Vec<(&'a Candidate, &FitRead)>| -> Vec<&'a Candidate> {
        candidates.into_iter().map(|(c, _)| c).collect()
    };
    let (set, verdict) = if !fits.is_empty() {
        (chosen(fits), "fits")
    } else if fits_only {
        (Vec::new(), "none")
    } else if !rewrite_only.is_empty() {
        (
            chosen(longest(rewrite_only, |read| {
                read.rung_runway_ms(FitWord::RewriteOnly)
            })),
            "rewrite_only",
        )
    } else if !least_bad.is_empty() {
        (least_bad, "least_bad")
    } else {
        (Vec::new(), "none")
    };
    let ending = reads
        .iter()
        .filter(|(_, read)| read.verdict == FitWord::Ending)
        .map(|(account_id, _)| account_id.clone())
        .collect();
    let step = fit_step(&reads, verdict);
    Gate {
        set,
        reads: if verdict == "fits" { reads } else { Vec::new() },
        step,
        ending,
    }
}

/// Step 4's rules in their order: spent, overage, return to plan, drain, shed. Each yields the siblings it
/// would move the pin to, and the first with any moves it under its own reason. Each rule appends its line: `no`
/// with the fact that kept it, or `fired` with its destinations and `not_run` for every rule after it. The spent
/// rule alone can answer `held`, for a reset inside the retry bound.
#[allow(clippy::too_many_arguments)]
fn move_of<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    current: &PinRow,
    key: &PinKey,
    eligible: &[&'a Candidate],
    window: &str,
    cold: bool,
    fast: bool,
    slowed: Option<&SlowDownRead>,
    rec: &mut Recorder,
) -> MoveVerdict<'a> {
    let (now, fallback_ms) = (request.now, request.fallback_ms);
    // Every shed line below carries the account's pace whichever rule answered.
    let slow_down_facts = slow_down_facts(state, pool, &current.account_id, slowed);
    let prefer_plan = pool.overage_policy == OveragePolicy::PreferPlan;
    let kind = |id: &str| snapshot.account(id).map(|a| a.kind);
    let plan_funded = |state: &RoutingState, c: &Candidate| {
        kind(&c.account_id) == Some(AccountKind::Oauth)
            && !paid_source(snapshot, &c.account_id)
            && !state.in_overage(&c.account_id, now, fallback_ms)
            && holds_allowance(state, snapshot, &c.account_id, window, now)
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
        rule_line(
            rec,
            MigrateReason::Spent,
            "no",
            vec![Fact::bool("fast", true)],
            &slow_down_facts,
        );
    } else if !pool.shedding.migrate_when_spent {
        rule_line(
            rec,
            MigrateReason::Spent,
            "no",
            vec![Fact::bool("migrate_when_spent", false)],
            &slow_down_facts,
        );
    } else if holds_allowance(state, snapshot, &current.account_id, window, now) {
        rule_line(
            rec,
            MigrateReason::Spent,
            "no",
            vec![Fact::bool("holds_allowance", true)],
            &slow_down_facts,
        );
    } else {
        // A reset inside the retry bound costs less than a cold rewrite: the request waits for it in place and the
        // pin renews once the window has refunded. The 429 that spent the window cools the account a margin past the
        // reset, so the wait runs to the later of the two, else the request would select again on a cooled account
        // and be placed afresh.
        let bound = now.saturating_add(seconds_ms(pool.failover.retry_in_place_seconds));
        let cooled_until = state
            .blocking_cooldown(
                &current.account_id,
                &request.canonical_model,
                now,
                request.fast,
            )
            .map(|row| row.until);
        // Once per source: a sample read during the wait can move the reset past the hold's end, and a second hold
        // on the account the request held on or attempted would repeat until the request's deadline, so the rule
        // continues to the move or the exhaustion instead. A slow-down hold is its own stage and spends no such wait:
        // a window spent after it still waits for its reset.
        let held_on = request.attempted.contains(&current.account_id)
            || request.holds.iter().any(|hold| {
                hold.account_id == current.account_id
                    && matches!(hold.reason, HoldReason::Cap | HoldReason::Reset)
            });
        if let Some((reset, spent_window, until)) =
            refunded_at(state, snapshot, &current.account_id, window, now)
                .filter(|_| !held_on)
                .map(|(reset, spent_window)| {
                    (
                        reset,
                        spent_window,
                        cooled_until.map_or(reset, |end| end.max(reset)),
                    )
                })
                .filter(|(_, _, until)| *until <= bound)
        {
            let wait_ms = until - now;
            rec.step(
                MigrateReason::Spent.step(),
                "held",
                vec![
                    Fact::bool("holds_allowance", false),
                    Fact::int("reset", reset).unit("time"),
                    Fact::text("window", spent_window),
                    Fact::int("wait_ms", wait_ms).unit("ms"),
                ],
            );
            not_run_after(rec, MigrateReason::Spent, &slow_down_facts);
            return MoveVerdict::Wait { until };
        }
        let gate = gate(
            state,
            snapshot,
            pool,
            request,
            current,
            eligible,
            window,
            &|c| {
                holds_allowance(state, snapshot, &c.account_id, window, now)
                    && allowance_known(state, snapshot, &c.account_id, window, now)
                    && !(prefer_plan && state.in_overage(&c.account_id, now, fallback_ms))
            },
            false,
            rec,
        );
        let facts = vec![Fact::bool("holds_allowance", false)];
        if let Some(moved) = concluded(rec, MigrateReason::Spent, facts, gate, &slow_down_facts) {
            return moved;
        }
    }

    // A subscription account in overage hands the session back to plan quota, warm or cold, onto a plan-funded
    // subscription sibling: one out of overage with allowance left, since a spent one would move the pin again
    // on its next request. While the evidence's reset falls within the grace the session keeps its cache: its
    // account is about to get its plan back, and moving now would move it twice.
    let current_kind = kind(&current.account_id);
    if request.fast {
        rule_line(
            rec,
            MigrateReason::Overage,
            "no",
            vec![Fact::bool("fast", true)],
            &slow_down_facts,
        );
    } else if !prefer_plan {
        rule_line(
            rec,
            MigrateReason::Overage,
            "no",
            vec![Fact::text("overage_policy", pool.overage_policy.as_str())],
            &slow_down_facts,
        );
    } else if current_kind != Some(AccountKind::Oauth) {
        rule_line(
            rec,
            MigrateReason::Overage,
            "no",
            vec![kind_fact(current_kind)],
            &slow_down_facts,
        );
    } else if !state.in_overage(&current.account_id, now, fallback_ms) {
        rule_line(
            rec,
            MigrateReason::Overage,
            "no",
            vec![Fact::bool("in_overage", false)],
            &slow_down_facts,
        );
    } else {
        let grace_ms = i64::from(pool.failover.overage_move_grace_minutes) * 60_000;
        let reset = state.overage(&current.account_id).and_then(|e| e.reset);
        let mut facts = vec![Fact::bool("in_overage", true)];
        match reset.filter(|reset| reset - now <= grace_ms) {
            Some(reset) => {
                facts.push(Fact::int("reset_in_grace", reset - now).unit("ms"));
                facts.push(Fact::int("grace_ms", grace_ms).unit("ms"));
                rule_line(rec, MigrateReason::Overage, "no", facts, &slow_down_facts);
            }
            None => {
                let gate = gate(
                    state,
                    snapshot,
                    pool,
                    request,
                    current,
                    eligible,
                    window,
                    &|c| plan_funded(state, c),
                    false,
                    rec,
                );
                if let Some(moved) =
                    concluded(rec, MigrateReason::Overage, facts, gate, &slow_down_facts)
                {
                    return moved;
                }
            }
        }
    }

    // A paid source bills every token in dollars (an api_key member, or a usage-based seat against its spend cap),
    // so a pin on one returns to plan quota as `return_to_plan` says: at once when cold under `always` or
    // `when_cold`, after the return dwell when warm under `always`, and never under `never`. A move writes a fresh
    // row, so `created_at` is the landing here. The member names no reset, so no grace applies.
    let dwell_age_ms = now - current.created_at;
    let mode = pool.failover.return_to_plan;
    if !paid_source(snapshot, &current.account_id) {
        rule_line(
            rec,
            MigrateReason::ReturnToPlan,
            "no",
            vec![kind_fact(current_kind), Fact::text("mode", mode.as_str())],
            &slow_down_facts,
        );
    } else {
        let (facts, returns) = match mode {
            ReturnToPlan::Never => (vec![Fact::text("mode", mode.as_str())], false),
            ReturnToPlan::WhenCold => (
                vec![
                    Fact::text("mode", mode.as_str()),
                    Fact::bool("cold", cold),
                    Fact::int("dwell_age_ms", dwell_age_ms).unit("ms"),
                ],
                cold,
            ),
            ReturnToPlan::Always => {
                let return_dwell_ms = return_to_plan_dwell_ms(pool);
                (
                    vec![
                        Fact::text("mode", mode.as_str()),
                        Fact::bool("cold", cold),
                        Fact::int("dwell_age_ms", dwell_age_ms).unit("ms"),
                        Fact::int("dwell_ms", return_dwell_ms).unit("ms"),
                    ],
                    cold || dwell_age_ms >= return_dwell_ms,
                )
            }
        };
        if !returns {
            rule_line(
                rec,
                MigrateReason::ReturnToPlan,
                "no",
                facts,
                &slow_down_facts,
            );
        } else {
            let gate = gate(
                state,
                snapshot,
                pool,
                request,
                current,
                eligible,
                window,
                &|c| plan_funded(state, c),
                false,
                rec,
            );
            if let Some(moved) = concluded(
                rec,
                MigrateReason::ReturnToPlan,
                facts,
                gate,
                &slow_down_facts,
            ) {
                return moved;
            }
        }
    }

    // A cold pin costs nothing to move off an account about to end. A warm one stays: past the end the
    // `subscription_ended` exclusion releases it and the upstream refuses the dead token.
    if !cold {
        rule_line(
            rec,
            MigrateReason::Drain,
            "no",
            vec![Fact::bool("cold", false)],
            &slow_down_facts,
        );
    } else if !winding_down(snapshot, pool, &current.account_id, now) {
        rule_line(
            rec,
            MigrateReason::Drain,
            "no",
            vec![Fact::bool("winding_down", false)],
            &slow_down_facts,
        );
    } else {
        let gate = gate(
            state,
            snapshot,
            pool,
            request,
            current,
            eligible,
            window,
            &|c| !winding_down(snapshot, pool, &c.account_id, now),
            false,
            rec,
        );
        let facts = vec![Fact::bool("cold", true), Fact::bool("winding_down", true)];
        if let Some(moved) = concluded(rec, MigrateReason::Drain, facts, gate, &slow_down_facts) {
            return moved;
        }
    }

    // The switch turns the round off and nothing else: the rules above, the fit gate and the reservations ran as they
    // do with it on.
    if !load_shedding(pool) {
        rule_line(
            rec,
            MigrateReason::Shed,
            "not_run",
            vec![Fact::text("round", "off")],
            &slow_down_facts,
        );
        return MoveVerdict::Stay;
    }
    // The shed rule's trigger, tested on every plan window the pool's pins on the account draw, in the windows' order,
    // one round per selection at most: on a pool that slows its pins the 5-hour window's is the stretch over the
    // threshold after the safety wait, every other window's the projection at its horizon over the limit the horizon's
    // bound sets after the projection's wait, a window inside its wait yielding to the next, as a 5-hour window inside
    // its hold does. A wait or the hold holds back the round alone, so a standing mark moves below while a window waits
    // or is held as it does under a fired one. The line names every projection it read whether or not one fired, with
    // its horizon and the wall instant its burn reaches, the stamp and the wait of each window inside its wait and the
    // 5-hour window's age and hold inside the hold. On a pool that sheds on the Fable reservation, the reservation's
    // test runs once no window fired, with a stamp of its own and the projection's wait, and its wait holds back the
    // round alone as a window's does.
    let source = current.account_id.as_str();
    let tier = snapshot.priced_tier(source).unwrap_or_default();
    let projections = projections_of(
        state,
        snapshot,
        pool,
        source,
        tier,
        now,
        BurnRead::Achieved,
        None,
    );
    let windows = trigger_windows(state, snapshot, pool, source);
    // A window with no reading this selection says nothing of its projection, so its stamp stands.
    for window in &windows {
        let Some(projection) = projections
            .get(*window)
            .filter(|_| projection_tested(pool, window))
        else {
            continue;
        };
        state.note_projection(
            &pool.id,
            source,
            window,
            projection.at_horizon > ceiling(pool, window),
            projection.reset,
            now,
        );
    }
    let reservation = if reservation_shedding(pool) {
        let read = reservation_read(state, snapshot, pool, source, now);
        let over = read.as_ref().is_some_and(ReservationRead::over);
        state.note_reservation(&pool.id, source, over, now);
        read
    } else {
        None
    };
    // The hold on the 5-hour round: each pin's burn average still carries the previous window's burst over the reset,
    // and the projection applies that rate to a horizon of up to five hours, so a round inside the first two averages
    // of a window would fire on the old window's burst. The age is read from the stored reading at each selection, so
    // the hold keeps no state, and the stamps above keep running through it.
    let hold_ms = burn_average_ms(pool).saturating_mul(2);
    let young = state
        .window_started(source, FIVE_HOUR, now)
        // A start at or after `now` is not a reading of a fresh window, so it reads no age and no hold, as a reading
        // without a reset does.
        .filter(|start| *start < now)
        .map(|start| now - start)
        .filter(|age| *age < hold_ms);
    let over_since = slowed
        .and(state.slow_down_state(&pool.id, source))
        .and_then(|entry| entry.over_since);
    let TriggerRead {
        fired: trigger,
        waiting,
    } = trigger_of(
        pool,
        &projections,
        slowed,
        over_since,
        young,
        &windows,
        &|window| state.projection_over_since(&pool.id, source, window),
        now,
    );
    let (trigger, reservation_waits) = match trigger {
        Some(trigger) => (Some(trigger), None),
        None => reservation_trigger(
            pool,
            &projections,
            reservation.as_ref(),
            state.reservation_over_since(&pool.id, source),
            slowed,
            now,
        ),
    };
    let mut facts = Vec::new();
    for window in &windows {
        let (Some(projection), Some(at), Some(until)) = (
            projections.get(*window),
            fit_key("projection", window),
            fit_key("horizon", window),
        ) else {
            continue;
        };
        facts.push(Fact::number(at, projection.at_horizon).unit("percent"));
        facts.push(Fact::int(until, projection.horizon).unit("time"));
        if let (Some(wall), Some(key)) = (wall_of(projection, now), fit_key("wall", window)) {
            facts.push(Fact::int(key, wall).unit("time"));
        }
        if let (Some(waits), Some(since), Some(wait)) = (
            waiting.iter().find(|waits| waits.window == *window),
            fit_key("over_since", window),
            fit_key("wait", window),
        ) {
            facts.push(Fact::int(since, waits.over_since).unit("time"));
            facts.push(Fact::int(wait, waits.wait_ms).unit("ms"));
        }
    }
    if let Some(age) = young {
        facts.push(Fact::int("age_five_hour", age).unit("ms"));
        facts.push(Fact::int("hold_five_hour", hold_ms).unit("ms"));
    }
    if let Some(read) = reservation.as_ref().filter(|read| read.over()) {
        facts.push(Fact::number("fable_reservation", read.reservation).unit("percent"));
    }
    if let Some(waits) = &reservation_waits {
        facts.push(Fact::int("over_since_fable_reservation", waits.over_since).unit("time"));
        facts.push(Fact::int("wait_fable_reservation", waits.wait_ms).unit("ms"));
    }
    // The line tests windows of both kinds, so it carries both ceilings.
    facts.push(Fact::number("five_hour_ceiling", pool.five_hour_ceiling).unit("percent"));
    facts.push(Fact::number("weekly_ceiling", pool.weekly_ceiling).unit("percent"));
    let stretch_waits = over_since.filter(|since| now - since < SLOW_DOWN_WAIT_MS);
    // A decision inside the safety wait says how long the stretch has read over the threshold.
    if let Some(since) = stretch_waits {
        facts.push(Fact::int("over_ms", now - since).unit("ms"));
    }
    rec.trigger_windows = windows;
    if let Some(trigger) = trigger {
        facts.push(Fact::text("trigger", trigger.window));
        facts.push(Fact::text("fired_by", trigger.source.as_str()));
        if let Some(excess) = trigger.source.excess() {
            facts.push(Fact::number("excess", excess * 3_600.0).unit("percent_per_hour"));
        }
        if state
            .last_shed(&pool.id, source)
            .is_none_or(|at| !round_in_dwell(pool, at, now))
        {
            let round = shed_round(
                state,
                snapshot,
                pool,
                request,
                eligible,
                current,
                cold,
                fast,
                now,
                fallback_ms,
                &trigger,
            );
            facts.push(Fact::text("round", "ran"));
            facts.push(Fact::number("target", round.facts.target).unit("percent_per_hour"));
            facts.push(Fact::list(
                "marks",
                round
                    .marks
                    .iter()
                    .map(|mark| pin_key_text(&mark.key))
                    .collect(),
            ));
            facts.push(Fact::list("skipped", round.skipped));
            for mark in &round.marks {
                state.mark_pin(&mark.key, now, trigger.window);
            }
            rec.shed_round = Some(ShedRoundRecord {
                account_id: source.to_string(),
                pin: key.clone(),
                facts: round.facts,
            });
            state.record_shed(&pool.id, source, now);
        } else {
            facts.push(Fact::text("round", "within_dwell"));
        }
    } else if waiting.is_empty()
        && stretch_waits.is_none()
        && young.is_none()
        && reservation_waits.is_none()
    {
        rule_line(rec, MigrateReason::Shed, "no", facts, &slow_down_facts);
        return MoveVerdict::Stay;
    }
    // A pin on an account cooled for this model is not renewed at step 5, so the fresh placement replaces it:
    // the release names the cooldown and the mark leaves with the pin, where a shed would name a move the
    // cooldown forced. A round that fired above has run, since a cooldown can hold one model alone.
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
        rule_line(rec, MigrateReason::Shed, "no", facts, &slow_down_facts);
        return MoveVerdict::Stay;
    }
    // A pin moves only when its session asks, so an unmarked pin never moves for utilization and a marked one
    // moves at its next request. A plan-quota round sheds window share, which a fast request does not draw, so
    // its mark moves a standard request alone; a dollar round's mark moves a fast one too.
    let marked = state.shed_marked(key);
    facts.push(Fact::bool("marked", marked));
    if !marked {
        rule_line(rec, MigrateReason::Shed, "no", facts, &slow_down_facts);
        return MoveVerdict::Stay;
    }
    if request.fast
        && burn_unit(state, snapshot, &current.account_id, now, fallback_ms)
            == BurnUnit::WindowShare
    {
        facts.push(Fact::bool("window_share_fast", true));
        rule_line(rec, MigrateReason::Shed, "no", facts, &slow_down_facts);
        return MoveVerdict::Stay;
    }
    // The shed rule's keep is the no-return test alone: a pin never returns inside the dwell to the account that
    // shed it. With nothing fitting the pin renews in place and its mark stands, rather than landing on an account
    // the next round would shed it from again.
    let gate = gate(
        state,
        snapshot,
        pool,
        request,
        current,
        eligible,
        window,
        &|c| !keeps_off(current, &c.account_id, pool, now),
        true,
        rec,
    );
    concluded(rec, MigrateReason::Shed, facts, gate, &slow_down_facts).unwrap_or(MoveVerdict::Stay)
}

/// The end of a rule whose reason held: the move where its gate found a destination, else its `no_destination` line
/// and `None`, so the next rule runs.
fn concluded<'a>(
    rec: &mut Recorder,
    rule: MigrateReason,
    facts: Vec<Fact>,
    gate: Gate<'a>,
    slow_down_facts: &[Fact],
) -> Option<MoveVerdict<'a>> {
    if gate.set.is_empty() {
        no_destination(rec, rule, facts, gate, slow_down_facts);
        return None;
    }
    Some(fired(rec, rule, facts, gate, slow_down_facts))
}

/// A rule that moves the pin: its line with the destinations, the rules after it `not_run`, then the fit table the
/// destinations were taken from. Answers the move to the destinations, which the strategy ranks, with the gate's
/// reads, whose runway breaks the strategy's tie.
fn fired<'a>(
    rec: &mut Recorder,
    rule: MigrateReason,
    mut facts: Vec<Fact>,
    gate: Gate<'a>,
    slow_down_facts: &[Fact],
) -> MoveVerdict<'a> {
    let Gate {
        set, reads, step, ..
    } = gate;
    facts.push(Fact::list(
        "destinations",
        set.iter().map(|c| c.account_id.clone()).collect(),
    ));
    rule_line(rec, rule, "fired", facts, slow_down_facts);
    not_run_after(rec, rule, slow_down_facts);
    rec.walk.push(step);
    MoveVerdict::Move(rule, set, reads)
}

/// A rule's line, the shed rule's with the slow-down facts the decision's read left beside its own, so every pinned
/// decision shows the account's pace whichever rule answered.
fn rule_line(
    rec: &mut Recorder,
    rule: MigrateReason,
    verdict: &'static str,
    mut facts: Vec<Fact>,
    slow_down_facts: &[Fact],
) {
    if rule == MigrateReason::Shed {
        facts.extend_from_slice(slow_down_facts);
    }
    rec.step(rule.step(), verdict, facts);
}

/// The lines of the rules after `rule`, `not_run` since `rule` answered.
fn not_run_after(rec: &mut Recorder, rule: MigrateReason, slow_down_facts: &[Fact]) {
    for later in MigrateReason::ORDER
        .into_iter()
        .skip_while(|r| *r != rule)
        .skip(1)
    {
        rule_line(rec, later, "not_run", Vec::new(), slow_down_facts);
    }
}

/// The shed line's slow-down facts: the read's figures on a pool that slows its pins, with the instant the stretch
/// went over the threshold while it stays over, `off` alone on a pool that does not, and nothing where the account
/// has no read.
fn slow_down_facts(
    state: &RoutingState,
    pool: &PoolRow,
    account_id: &str,
    read: Option<&SlowDownRead>,
) -> Vec<Fact> {
    if !slow_down(pool) {
        return vec![Fact::text("slow_down", "off")];
    }
    let Some(read) = read else {
        return Vec::new();
    };
    let mut facts = vec![
        Fact::text("slow_down", "on"),
        Fact::int("stretch_ms", read.stretch_ms).unit("ms"),
        Fact::int("threshold_ms", read.threshold_ms).unit("ms"),
        Fact::number("factor", read.factor),
        // The share of the factor the holds apply, so the line reads what the demand projected, how much of it applied
        // and the speed realized, in that order.
        Fact::number("ramp", read.ramp).unit("percent"),
        // The speed this read's plan realizes, the one the hold's outcome names, so the line and the card read one.
        Fact::number("speed", read.realized),
        Fact::number("demand_five_hour", read.demand * 3_600.0).unit("percent_per_hour"),
    ];
    if let Some(since) = state
        .slow_down_state(&pool.id, account_id)
        .and_then(|entry| entry.over_since)
    {
        facts.push(Fact::int("over_since", since).unit("time"));
    }
    facts
}

/// Step 4's read of the pinned account's slow-down and the write of its entry, `None` where the account has no read.
/// The speed the read's plan realizes is the one figure the selection folds into the entry: the hold reads its pin's
/// part of the plan and writes no speed, so no second write lands at the same instant.
fn slow_down_read(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    now: i64,
) -> Option<SlowDownRead> {
    let tier = snapshot.priced_tier(account_id).unwrap_or_default();
    let read = slow_down_of(state, snapshot, pool, account_id, tier, now)?;
    state.write_slow_down(
        &pool.id,
        account_id,
        &read,
        read.realized,
        now,
        burn_average_ms(pool),
    );
    Some(read)
}

/// A slow-down hold for the requesting pin: its deadline, the hold's length from the arrival, and the pin's cycle it
/// was sized from.
struct HoldPlan {
    until: i64,
    hold_ms: i64,
    cycle_ms: Option<i64>,
}

/// The requesting pin's hold, read from the plan its selection's read computed, measured from the arrival so a
/// selection that queued adds nothing to it. The hold ends at the earlier of its part's end and the reset the read
/// names, with no slack since the ceiling sits under the limit. Writes no speed. `None` when the pin has no part in
/// the plan, its hold so bounded reads under `SLOW_DOWN_MIN_HOLD_MS`, or its end is already past, as for a selection
/// that queued longer than its own hold.
fn slow_down_hold(request: &SelectRequest, key: &PinKey, read: &SlowDownRead) -> Option<HoldPlan> {
    let part = read.plan.iter().find(|part| part.key == *key)?;
    let end = request.received_at.saturating_add(part.hold_ms);
    let until = read.reset.map_or(end, |reset| end.min(reset));
    let hold_ms = until - request.received_at;
    (hold_ms >= SLOW_DOWN_MIN_HOLD_MS && until > request.now).then_some(HoldPlan {
        until,
        hold_ms,
        cycle_ms: Some(part.cycle_ms),
    })
}

/// The hold of a moved pin's request that arrives before the head of the request that carried the move. The
/// upstream makes a cache entry readable only once the response that writes it has begun, so a second request out
/// before that head writes the prefix again at the write multiplier, where the round and the move's reservation
/// priced one rewrite. Bounded by the pool's hold cap from the first request's dispatch whatever the slow-down
/// switch, since the cap bounds a wait before a head and the switch governs a slowed pin's sizing alone, and the
/// relay bounds it by the request's deadline. Nothing for a request that already waited (the relay's re-selection
/// after any wait), a token count (it writes no cache), the first request itself, or a bound under the floor, since
/// the head is due. Every other request of the pin waits, the summary fork, a fast request, a probe, a non-portable
/// turn bound at step 3 and an api_key landing among them: the race costs the same tokens twice on each. The hold
/// moves no dispatch: the head may release it long before the bound, and the renewal after it dates the dispatch.
fn rewrite_answer(
    state: &RoutingState,
    pool: &PoolRow,
    request: &SelectRequest,
    current: &PinRow,
    key: &PinKey,
) -> Option<HoldFacts> {
    if request.count || !request.holds.is_empty() {
        return None;
    }
    let mark = state
        .head_mark(key)
        .filter(|mark| mark.request_id != request.request_id)?;
    let until = mark.at + slow_down_hold_ms(pool);
    if until - request.now < SLOW_DOWN_MIN_HOLD_MS {
        return None;
    }
    Some(hold_facts(
        request,
        &current.account_id,
        HoldReason::Rewrite,
        Some(until),
        Some(mark.request_id.clone()),
    ))
}

/// Step 5's slow-down hold for a pin step 4 left in place on an eligible account, `None` where the pin renews. Only a
/// request of a held class that carries no hold is held: the second selection after any wait goes on. A deadline
/// standing on the pin is rejoined to the same instant, whatever the pool's switch reads now, since the hold was
/// answered under the setting then in force: no hold is sized, nothing is folded and the span counts the wait once.
/// The exception is a 5-hour reset passed since the deadline stood: step 4's read that replaces the old window's
/// entry, or the purge tick that removed it first, has dropped the deadline, so the request is read afresh. Otherwise
/// the pin waits its part of the read's plan while the read slows the account, the deadline standing on the pin for
/// the requests that follow.
fn slow_down_answer(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    current: &PinRow,
    key: &PinKey,
    rec: &mut Recorder,
) -> Option<HoldFacts> {
    let kind = snapshot.account(&current.account_id).map(|a| a.kind);
    if !slowable(request, kind) || !request.holds.is_empty() {
        return None;
    }
    let pace = state.pin_pace(key)?;
    let standing = pace.standing_deadline(request.now);
    let plan = match standing {
        Some(deadline) => HoldPlan {
            until: deadline,
            hold_ms: deadline - request.received_at,
            cycle_ms: pace.cycle_ms.map(|cycle| cycle.round() as i64),
        },
        None => {
            let read = rec.slow_down.as_ref().filter(|read| read.factor < 1.0)?;
            let plan = slow_down_hold(request, key, read)?;
            state.stand_slow_down_deadline(key, plan.until);
            state.add_slow_down_held(&pool.id, &current.account_id, plan.hold_ms);
            plan
        }
    };
    if let Some(pin) = rec.pin.as_mut() {
        pin.slow_down_until = Some(plan.until);
    }
    let hold = hold_facts(
        request,
        &current.account_id,
        HoldReason::SlowDown,
        Some(plan.until),
        None,
    );
    rec.hold_plan = Some(plan);
    Some(hold)
}

/// A hold on `account_id` as the selector answers it. The wait and whether its bound ran out are the relay's to read,
/// once it has waited.
fn hold_facts(
    request: &SelectRequest,
    account_id: &str,
    reason: HoldReason,
    until: Option<i64>,
    waits_for: Option<String>,
) -> HoldFacts {
    HoldFacts {
        account_id: account_id.to_string(),
        cohort: None,
        held_ms: 0,
        attempt: request.attempt,
        seq: request.seq,
        ran_out: false,
        reason,
        until,
        waits_for,
        pin_ended: request.pin_ended,
    }
}

/// The holds of the account's pins with a cycle at `factor` under the cap, and the speed every kept pin realizes
/// together. Each pin `(key, cycle, weight)` with a cycle wants `c x (1 / f - 1)`. A pin without a cycle is never held
/// and runs at full speed, and a pin that wants more than the cap waits the cap and runs at `c / (c + cap)`, both above
/// the factor, so the rest share what is left of the reduction at one speed, the step repeated until no pin crosses the
/// cap, and one speed per account holds as far as the cap lets it. A pin whose hold reads under the floor is not held
/// and counts at full speed. Weights that all read zero (nothing priced, or each burn decayed past its cutoff) weigh
/// the pins equally.
fn water_fill(
    pins: &[(PinKey, Option<f64>, f64)],
    factor: f64,
    cap_ms: i64,
) -> (Vec<PinHold>, f64) {
    if pins.is_empty() {
        return (Vec::new(), 1.0);
    }
    let factor = factor.clamp(0.0, 1.0);
    let cap = cap_ms as f64;
    let weights: Vec<f64> = if pins.iter().all(|(_, _, weight)| *weight <= 0.0) {
        vec![1.0; pins.len()]
    } else {
        pins.iter().map(|(_, _, weight)| weight.max(0.0)).collect()
    };
    let total: f64 = weights.iter().sum();
    let unheld: f64 = pins
        .iter()
        .zip(&weights)
        .filter(|((_, cycle, _), _)| cycle.is_none())
        .map(|(_, weight)| weight)
        .sum();
    let wanted = |cycle: f64, speed: f64| {
        if speed > 0.0 {
            cycle * (1.0 / speed - 1.0)
        } else {
            f64::INFINITY
        }
    };
    let mut capped = vec![false; pins.len()];
    let mut rest_speed = factor;
    loop {
        let capped_share: f64 = pins
            .iter()
            .zip(&weights)
            .zip(&capped)
            .filter_map(|(((_, cycle, _), weight), capped)| {
                (*cycle)
                    .filter(|_| *capped)
                    .map(|cycle| weight * cycle / (cycle + cap))
            })
            .sum();
        let rest_weight: f64 = pins
            .iter()
            .zip(&weights)
            .zip(&capped)
            .filter(|(((_, cycle, _), _), capped)| cycle.is_some() && !**capped)
            .map(|((_, weight), _)| weight)
            .sum();
        if rest_weight <= 0.0 {
            break;
        }
        rest_speed = ((factor * total - unheld - capped_share) / rest_weight).clamp(0.0, 1.0);
        let mut crossed = false;
        for ((_, cycle, _), capped) in pins.iter().zip(capped.iter_mut()) {
            if cycle.is_some_and(|cycle| !*capped && wanted(cycle, rest_speed) > cap) {
                *capped = true;
                crossed = true;
            }
        }
        if !crossed {
            break;
        }
    }
    let mut realized = unheld;
    let plan = pins
        .iter()
        .zip(&weights)
        .zip(&capped)
        .filter_map(|(((key, cycle, _), weight), capped)| {
            let cycle = (*cycle)?;
            // The cap bounds a pin of no weight too, which the loop leaves at the factor when no weighted pin is left
            // to share the reduction.
            let hold = if *capped {
                cap
            } else {
                wanted(cycle, rest_speed).min(cap)
            };
            let hold_ms = hold.round() as i64;
            realized += weight
                * if hold_ms < SLOW_DOWN_MIN_HOLD_MS {
                    1.0
                } else {
                    cycle / (cycle + hold)
                };
            Some(PinHold {
                key: key.clone(),
                cycle_ms: cycle.round() as i64,
                hold_ms,
            })
        })
        .collect();
    (plan, realized / total)
}

/// A rule whose reason held but which found no sibling to move the pin to, with the candidates the end exclusion
/// dropped where it dropped any: the drain's gate reads no candidate inside the wind-down, so its line names no list.
/// The fit table follows when the gate read any candidate, so the record says why none passed.
fn no_destination(
    rec: &mut Recorder,
    rule: MigrateReason,
    mut facts: Vec<Fact>,
    gate: Gate<'_>,
    slow_down_facts: &[Fact],
) {
    facts.push(Fact::bool("no_destination", true));
    if !gate.ending.is_empty() {
        facts.push(Fact::list("ending", gate.ending));
    }
    rule_line(rec, rule, "no", facts, slow_down_facts);
    if !gate.step.facts.is_empty() {
        rec.walk.push(gate.step);
    }
}

/// The unit the account's burn is shed in: the plan window's share on a subscription account on plan quota,
/// dollars on an api_key account, a usage-based seat or one in overage.
pub(crate) fn burn_unit(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    now: i64,
    fallback_ms: i64,
) -> BurnUnit {
    let kind = snapshot
        .account(account_id)
        .map_or(AccountKind::Oauth, |a| a.kind);
    BurnUnit::of(
        kind,
        snapshot.usage_based(account_id),
        state.in_overage(account_id, now, fallback_ms),
    )
}

/// What fired a round: the first plan window, in the trigger's order, whose test read over past its wait, else the
/// Fable reservation's test past its wait on `seven_day`.
pub(crate) struct Trigger {
    pub window: &'static str,
    /// The prediction at the round's instant.
    pub utilization: f64,
    pub projection: f64,
    pub horizon: i64,
    /// The limit the test read: the wall where the projection's horizon is the account's end or the window's own
    /// reset, the window's ceiling at the five-hour bound and under the stretch.
    pub ceiling: f64,
    pub source: TriggerSource,
    /// The account's factor and the speed its demand was read at, where the account has a slow-down read.
    pub factor: Option<f64>,
    pub speed: Option<f64>,
}

/// The test that fired a round, with the figures that test alone reads. `excess` is share per second on the trigger's
/// window.
#[derive(Clone, Copy)]
pub(crate) enum TriggerSource {
    /// The account's 7-day room read under its Fable reservation. The test reads a room and no rate, so its round's
    /// target is the shed cap's share of the walked burn alone.
    FableReservation { reservation: f64 },
    /// The projection at the window's horizon.
    Projection { excess: f64 },
    /// The stretch, with the account's stretch and the pool's threshold.
    SlowDown {
        excess: f64,
        stretch_ms: i64,
        threshold_ms: i64,
    },
}

impl TriggerSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TriggerSource::FableReservation { .. } => "fable_reservation",
            TriggerSource::Projection { .. } => "projection",
            TriggerSource::SlowDown { .. } => "slow_down",
        }
    }

    pub(crate) fn excess(self) -> Option<f64> {
        match self {
            TriggerSource::FableReservation { .. } => None,
            TriggerSource::Projection { excess } | TriggerSource::SlowDown { excess, .. } => {
                Some(excess)
            }
        }
    }

    pub(crate) fn fable_reservation(self) -> Option<f64> {
        match self {
            TriggerSource::FableReservation { reservation } => Some(reservation),
            _ => None,
        }
    }

    pub(crate) fn stretch_ms(self) -> Option<i64> {
        match self {
            TriggerSource::SlowDown { stretch_ms, .. } => Some(stretch_ms),
            _ => None,
        }
    }

    pub(crate) fn threshold_ms(self) -> Option<i64> {
        match self {
            TriggerSource::SlowDown { threshold_ms, .. } => Some(threshold_ms),
            _ => None,
        }
    }
}

/// A mark the round made: the key of the pin it marked.
pub(crate) struct Mark {
    pub key: PinKey,
}

/// What a round made of one pin: its skip word, or the figures it read on the destination it planned.
#[derive(Clone, Default)]
struct PinRead {
    skip: Option<&'static str>,
    /// The move at the destination `plan_of` found for the pin, or with none where it found none.
    moved: Option<MoveRead>,
    rewrite: Option<f64>,
    rewrite_window: Option<&'static str>,
    runway_ms: Option<i64>,
}

/// A round's answer: its facts, the marks it made and the skips as `key: reason` texts for the shed line.
struct Round {
    facts: ShedRoundFacts,
    marks: Vec<Mark>,
    skipped: Vec<String>,
}

/// The plan windows a round on `source` tests, in the trigger's order: both shared windows, then each model week of
/// `MODEL_WINDOWS` that a pin of the pool on the account draws, since a week no pin draws has no burn to shed.
fn trigger_windows(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    source: &str,
) -> Vec<&'static str> {
    let drawn: Vec<&'static str> = state
        .pins_on(source, &in_pool(snapshot, pool))
        .iter()
        .map(|p| window_for(&p.model))
        .collect();
    [FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .chain(
            MODEL_WINDOWS
                .iter()
                .map(|(_, window)| *window)
                .filter(|window| drawn.contains(window)),
        )
        .collect()
}

/// Whether the projection test reads `window` on `pool`: every window but the 5-hour one on a pool that slows its pins,
/// whose test there is the stretch.
fn projection_tested(pool: &PoolRow, window: &str) -> bool {
    !(window == FIVE_HOUR && slow_down(pool))
}

/// What `trigger_of` read: the window that fired, if one did, and the windows of the projection test inside their wait,
/// in the order it tested them.
struct TriggerRead {
    fired: Option<Trigger>,
    waiting: Vec<Waiting>,
}

/// A window of the projection test that reads over its limit and has not read over for its wait yet.
struct Waiting {
    window: &'static str,
    /// When the window first read over the ceiling.
    over_since: i64,
    wait_ms: i64,
}

/// What fires a round, if anything: the first of `windows` whose test reads over past its wait, with the windows of the
/// projection test that read over inside their wait before it. The excess is share per second on the fired window: the
/// burn that, removed now, lands the projection at the horizon on the limit the test read, and on a span the band
/// crosses, the average rate to remove over the span.
///
/// On a pool that slows its pins the 5-hour window's test is the stretch: it fires once `over_since`, the instant the
/// stretch first read over the threshold, is `SLOW_DOWN_WAIT_MS` past, with the horizon at the reset less the
/// threshold and the demand's projection there over the ceiling. Removing the excess then brings the remaining
/// demand's stretch to the threshold, so the round takes what the slow-down cannot cover. A reset nearer than the
/// threshold puts that horizon at or before `now` and fires nothing, since the slow-down alone holds the window to its
/// reset, and an account with no read fires nothing on the window. Every other window's test is the projection at its
/// horizon over the limit the horizon's bound sets, strictly, on the achieved burn: the wall, 1.0, where the horizon is
/// the account's end or the window's own reset, since the quota left there is lost, and the window's ceiling at the
/// five-hour bound. It fires once the window has read over since `stamp` names for `min((wall - over_since) / 4, 2 *
/// burn_average_ms)`, `wall` being the wall instant, where the projection's burn reaches 1.0 as `wall_of` reads it now,
/// and the wait the cap alone while no burn reaches it, so a burst toward a near wall instant fires early, a slow climb
/// waits, and one large turn's charge, which moves the prediction before the burn average has absorbed it, fires no
/// round at the selection it lands on. The stamp stays the first reading over the window's own ceiling, so a window
/// that crosses the wall at an end or a reset horizon has spent the part of its wait it read between the two lines,
/// which confirms the climb as the wait would. On a weekly window at a ceiling of 1.0 the two lines are one, so its
/// wait runs whole from the crossing. Replayed over the recorded projection at the default burn average, with the
/// ceiling as the line at every horizon, this wait fires 12 rounds while the window reads under the ceiling (2 on a
/// window still under it at the horizon) where a quarter of the time to the horizon capped at the dwell fires 56 (33),
/// falsely fires on 1 window of 32 against 11, costs 170,044 tokens of rewrites and missed wall instants against
/// 6,990,451, and fires 2 minutes before a wall instant the horizon's quarter misses and 159 minutes before another. A
/// window inside its wait yields to the next.
///
/// A 5-hour window whose age `young` names, under twice the pool's burn average, fires under neither test and yields to
/// the next window as a waiting one does, since the burn average still carries the previous window's burst over the
/// reset. Its wait is read first, so the line names the wait beside the age where both hold.
#[allow(clippy::too_many_arguments)]
fn trigger_of(
    pool: &PoolRow,
    projections: &BTreeMap<String, Projection>,
    read: Option<&SlowDownRead>,
    over_since: Option<i64>,
    young: Option<i64>,
    windows: &[&'static str],
    stamp: &dyn Fn(&str) -> Option<i64>,
    now: i64,
) -> TriggerRead {
    let mut waiting = Vec::new();
    let mut test = |window: &'static str| -> Option<Trigger> {
        if window == FIVE_HOUR && slow_down(pool) {
            let limit = ceiling(pool, window);
            let read = read?;
            over_since.filter(|since| now - since >= SLOW_DOWN_WAIT_MS)?;
            let horizon = read.horizon - read.threshold_ms;
            if horizon <= now {
                return None;
            }
            let to_horizon = seconds(horizon - now);
            let projection = read.utilization + read.banded.increase_until(horizon);
            // A burn of nothing at the ceiling reads a stretch of the whole time to the reset with nothing to remove.
            return (projection > limit).then_some(Trigger {
                window,
                utilization: read.utilization,
                projection,
                horizon,
                ceiling: limit,
                source: TriggerSource::SlowDown {
                    excess: (projection - limit) / to_horizon.max(1.0),
                    stretch_ms: read.stretch_ms,
                    threshold_ms: read.threshold_ms,
                },
                factor: Some(read.factor),
                speed: Some(read.speed),
            });
        }
        let projection = projections
            .get(window)
            .filter(|projection| projection.at_horizon > projection.limit)?;
        if let Some(waits) = inside_wait(pool, window, stamp(window), wall_of(projection, now), now)
        {
            waiting.push(waits);
            return None;
        }
        let source = TriggerSource::Projection {
            // A horizon at `now` would divide by nothing: the excess then reads the whole overrun as one second's.
            excess: (projection.at_horizon - projection.limit)
                / seconds(projection.horizon - now).max(1.0),
        };
        Some(projection_trigger(window, projection, source, read))
    };
    // The hold on the 5-hour round sits after the window's test so the line names the wait beside the age where both
    // hold. A window younger than twice the pool's burn average fires under neither source and yields to the next
    // window, since the burn average still carries the previous window's burst over the reset.
    let fired = windows
        .iter()
        .find_map(|window| test(window).filter(|_| !(*window == FIVE_HOUR && young.is_some())));
    TriggerRead { fired, waiting }
}

/// The wait of a test that has read over since `stamp`, `min((wall - stamp) / 4, 2 * burn_average_ms)`, the cap alone
/// where no `wall` is named: `Some` while `now` reads inside it.
fn inside_wait(
    pool: &PoolRow,
    window: &'static str,
    stamp: Option<i64>,
    wall: Option<i64>,
    now: i64,
) -> Option<Waiting> {
    // The shed rule stamps a test before it runs, so a missing stamp starts the wait now.
    let since = stamp.unwrap_or(now);
    let cap = burn_average_ms(pool).saturating_mul(2);
    let wait_ms = wall.map_or(cap, |wall| (wall.saturating_sub(since) / 4).min(cap));
    (now - since < wait_ms).then_some(Waiting {
        window,
        over_since: since,
        wait_ms,
    })
}

/// The round `source` fires on `window`, its figures read off the window's projection and the slow-down's factor and
/// speed where the account has a read.
fn projection_trigger(
    window: &'static str,
    projection: &Projection,
    source: TriggerSource,
    slowed: Option<&SlowDownRead>,
) -> Trigger {
    Trigger {
        window,
        utilization: projection.predicted,
        projection: projection.at_horizon,
        horizon: projection.horizon,
        ceiling: projection.limit,
        source,
        factor: slowed.map(|read| read.factor),
        speed: slowed.map(|read| read.speed),
    }
}

/// The Fable reservation's test on an account: its 7-day room and the reservation Pace holds back from that room for a
/// `seven_day` request.
struct ReservationRead {
    room: f64,
    reservation: f64,
}

impl ReservationRead {
    /// Strictly under, since a room equal to the reservation still holds the whole of it.
    fn over(&self) -> bool {
        self.room < self.reservation
    }
}

/// `account_id`'s reservation test under `pool`, read through `pace_of` as the pools route reads the member's score,
/// `None` where no reservation applies: a paid source, an account with no Fable reading, a pricer with no factor for
/// the Fable week.
fn reservation_read(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    now: i64,
) -> Option<ReservationRead> {
    let pace = pace_of(
        state,
        snapshot,
        pool,
        account_id,
        snapshot.incarnation(account_id).unwrap_or(0),
        SEVEN_DAY,
        now,
        None,
    );
    pace.fable_reservation.map(|reservation| ReservationRead {
        room: pace.remaining,
        reservation,
    })
}

/// What the Fable reservation's test fires once every window's test fired nothing: a round on `seven_day` once the
/// account's room has read under its reservation since `since` for the projection's wait,
/// `min((reset - since) / 4, 2 * burn_average_ms)` with `reset` the 7-day window's, the cap alone where its reading
/// names none, else the stamp and the wait while the room reads short inside it. The round reads its window's figures
/// off the 7-day projection as a projection round does, so an account with no 7-day projection fires nothing.
fn reservation_trigger(
    pool: &PoolRow,
    projections: &BTreeMap<String, Projection>,
    read: Option<&ReservationRead>,
    since: Option<i64>,
    slowed: Option<&SlowDownRead>,
    now: i64,
) -> (Option<Trigger>, Option<Waiting>) {
    let (Some(read), Some(projection)) =
        (read.filter(|read| read.over()), projections.get(SEVEN_DAY))
    else {
        return (None, None);
    };
    if let Some(waits) = inside_wait(pool, SEVEN_DAY, since, projection.reset, now) {
        return (None, Some(waits));
    }
    let source = TriggerSource::FableReservation {
        reservation: read.reservation,
    };
    (
        Some(projection_trigger(SEVEN_DAY, projection, source, slowed)),
        None,
    )
}

/// What the heal check read when it cleared a mark: the healing projection, with the stretch and the threshold where
/// the stretch healed it, else the line the projection read under.
pub(crate) enum Healed {
    Stretch {
        projection: f64,
        stretch_ms: i64,
        threshold_ms: i64,
    },
    Projection {
        projection: f64,
        /// The line the healing projection read under: 1.0 where the heal's horizon was bound to the wall, the
        /// window's ceiling where it was bound to the span.
        limit: f64,
    },
}

/// Whether a marked pin's window reads back under the round's test at `now`, the test the window fires on. A 5-hour
/// mark on a pool that slows its pins heals when the stretch reads at or under the threshold, read here since the heal
/// runs ahead of the selection's own read, and answers the demand's projection at the reset less the threshold, or the
/// prediction where that instant has passed, since no horizon lies ahead to price. Every other mark heals when the
/// projection on its window at its horizon, on the achieved burn, reads under the round's limit for that horizon,
/// strictly, since a reading at the line leaves the pin's next turn no room: the wall at the account's end or the
/// window's own reset, where the quota left is lost, and the ceiling at the five-hour bound. On a pool that sheds on
/// the Fable reservation, a `seven_day` mark on a pin whose model draws no model week also stands while the account's
/// 7-day room reads under its reservation, since a mark of the reservation's round would otherwise heal on the
/// projection alone before its pin moved. `None` for a mark that stands or whose window the account has no read of.
fn healed(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    pin: &PinRow,
    now: i64,
) -> Option<Healed> {
    let window = pin.mark.as_ref()?.window.as_str();
    let tier = snapshot.priced_tier(&pin.account_id).unwrap_or_default();
    if window == FIVE_HOUR && slow_down(pool) {
        let read = slow_down_of(state, snapshot, pool, &pin.account_id, tier, now)?;
        if read.stretch_ms > read.threshold_ms {
            return None;
        }
        return Some(Healed::Stretch {
            projection: read.utilization
                + read.banded.increase_until(read.horizon - read.threshold_ms),
            stretch_ms: read.stretch_ms,
            threshold_ms: read.threshold_ms,
        });
    }
    let projection = projection_of(
        state,
        snapshot,
        pool,
        &pin.account_id,
        tier,
        window,
        now,
        BurnRead::Achieved,
    )?;
    // The projection's verdict first, so a mark it holds reads no reservation.
    let heals = projection.at_horizon < projection.limit
        && !(window == SEVEN_DAY
            && model_week(&pin.model).is_none()
            && reservation_shedding(pool)
            && reservation_read(state, snapshot, pool, &pin.account_id, now)
                .is_some_and(|read| read.over()));
    heals.then_some(Healed::Projection {
        projection: projection.at_horizon,
        limit: projection.limit,
    })
}

/// The facts of `mark`, as the decision clears it for `reason`, with what the heal read on `healed`.
pub(crate) fn mark_cleared_of(
    mark: &PinMark,
    reason: &'static str,
    heal: Option<&Healed>,
) -> MarkClearedFacts {
    let facts = MarkClearedFacts {
        reason,
        marked_at: mark.at,
        marked_on: mark.on.clone(),
        marked_window: mark.window.clone(),
        projection: None,
        stretch_ms: None,
        threshold_ms: None,
        limit: None,
    };
    match heal {
        None => facts,
        Some(&Healed::Stretch {
            projection,
            stretch_ms,
            threshold_ms,
        }) => MarkClearedFacts {
            projection: Some(projection),
            stretch_ms: Some(stretch_ms),
            threshold_ms: Some(threshold_ms),
            ..facts
        },
        Some(&Healed::Projection { projection, limit }) => MarkClearedFacts {
            projection: Some(projection),
            limit: Some(limit),
            ..facts
        },
    }
}

/// The request `fit_of` and `pin_figures` read a pin of the round by. The request's own pin reads the request as it
/// is. Another pin reads the request's shape with its own model and lane, which name the windows it draws and its
/// speed, and no body estimate, since a pin never renewed with usage has no prompt to rewrite. It keeps the request's
/// candidates, resolved for the request's model: a pin of another model is planned among them because the plan is
/// advisory, the pin's own next request runs the gate over its own candidates, and the cost of the shortcut is a
/// wrong `no_runway` skip or a wrong planned destination, never a wrong move.
fn pin_request(request: &SelectRequest, pin: &PinRow, current: bool) -> SelectRequest {
    if current {
        return request.clone();
    }
    SelectRequest {
        canonical_model: pin.model.clone(),
        folded_model: pin.model.clone(),
        lane: pin.lane.clone(),
        fast: lane_is_fast(&pin.lane),
        entrypoint: pin.entrypoint.clone(),
        body_tokens: 0,
        ..request.clone()
    }
}

/// The request a standing pin is read by outside a round, shaped from the pin's own facts: its key and generation, its
/// agent and session, its model, its lane with the lane's speed and its entrypoint. `pin_request` takes the same model,
/// lane and entrypoint for another pin of a round and keeps the round request's key, generation, agent and session. No
/// request stands behind a standing, so it holds no body estimate and no candidates, and every other member reads
/// empty.
fn standing_request(pin: &PinRow) -> SelectRequest {
    // Every member is written out, so a member the type gains states its value for a standing too.
    SelectRequest {
        request_id: String::new(),
        attempt: 0,
        seq: 0,
        key_id: pin.key_id.clone(),
        key_generation: pin.key_generation,
        agent_id: pin.agent_id.clone(),
        session_id: pin.session_id.clone(),
        canonical_model: pin.model.clone(),
        folded_model: pin.model.clone(),
        candidates: Vec::new(),
        portable: false,
        credit_account: None,
        attempted: Vec::new(),
        now: 0,
        received_at: 0,
        fallback_ms: 0,
        revision: 0,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        boundaries: Arc::from([]),
        client: ClientClass::Other,
        entrypoint: pin.entrypoint.clone(),
        static_boundaries: Arc::from([]),
        marker: false,
        parent_agent_id: None,
        holds: Vec::new(),
        failed_over: None,
        pin_ended: false,
        dialed: false,
        held_out: false,
        lane: pin.lane.clone(),
        fast: lane_is_fast(&pin.lane),
        classifier: false,
        body_tokens: 0,
        effort: None,
    }
}

/// What a shedding round reads of a pin's move to one destination. The round's order, its pick and the standing
/// pins take their figures from this one value, so the sessions route answers what a round reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveRead {
    /// The cost the pick compares for a warm pin: the Pro share the rewrite spends on the week the pin's model
    /// draws, plus `drop_cost`. None where neither is priced above zero, as for a pin with no prompt size.
    pub cost: Option<f64>,
    /// The size the order divides a warm pin's burn by, in tokens: the rewrite at the destination plus `drop_size`,
    /// never under one token. None where the pin holds no prompt size.
    pub size: Option<f64>,
    /// The dropped thinking at the weight of output, in tokens: the size the order divides a cold pin's burn by.
    /// None where the move drops no thinking, a count of zero included, and where the pricer answers no weight.
    pub drop_size: Option<f64>,
    /// The Pro share of the dropped thinking as output on the window `cost` reads: the cost the pick compares for a
    /// cold pin. None where `drop_size` is.
    pub drop_cost: Option<f64>,
    /// The thinking the move drops, in tokens: the pin's sum for its own account where its pair with the destination
    /// reads bound. None where the pair reads kept, where the model has no observation and with no destination.
    pub dropped_thinking_tokens: Option<u64>,
}

/// The round's read of `pin`'s move to `destination`, or with none where no destination stands. The write's share is
/// the Pro share the rewrite spends on the week the pin's model draws: the week is the scarce window, since the 5-hour
/// window refills, and a Fable pin's cost carries its week's factor. The share is the pin's own on every kind of
/// destination, since a figure that read 0 on one kind of sibling would turn the pick into a destination preference,
/// which is the gate's to make. The dropped thinking reads the pair of the pin's account and the destination, and its
/// weight is the share of a token as output over its share as a cache write at the pin's TTL, so the cost model sets
/// it. `req` is the request `pin_request` shapes for the pin in a round, or `standing_request`'s for a standing pin.
pub(crate) fn move_read(
    state: &RoutingState,
    req: &SelectRequest,
    pin: &PinRow,
    now: i64,
    destination: Option<&str>,
) -> MoveRead {
    let model = req.canonical_model.as_str();
    let share = |usage: PinUsage| {
        state.share().equivalent(
            window_for(model),
            req.entrypoint.as_deref(),
            model,
            usage,
            now,
        )
    };
    let dropped_thinking_tokens = bound_destination(state, pin, destination).map(|_| {
        pin.thinking_sums
            .get(&pin.account_id)
            .map_or(0, |sum| sum.tokens)
    });
    // An output share of zero weighs the drop at nothing, so the read answers none there: the pin stays in the first
    // part of the cold group, and no zero reaches `move_drop_size` or the order's division.
    let weighed = dropped_thinking_tokens
        .filter(|count| *count > 0)
        .and_then(|count| {
            let as_output = share(PinUsage {
                output: count,
                ..PinUsage::default()
            })
            .filter(|share| *share > 0.0)?;
            let as_write =
                share(cache_write(count, pin.cache_ttl_ms)).filter(|share| *share > 0.0)?;
            Some((count as f64 * as_output / as_write, as_output))
        });
    let drop_size = weighed.map(|(size, _)| size);
    let drop_cost = weighed.map(|(_, cost)| cost);
    let sized = prompt_size(state, pin, req, destination);
    let size = (prompt_size(state, pin, req, None) > 0)
        .then(|| (sized as f64 + drop_size.unwrap_or(0.0)).max(1.0));
    let write = share(cache_write(sized, pin.cache_ttl_ms)).filter(|share| *share > 0.0);
    let cost = match (write, drop_cost) {
        (None, None) => None,
        (write, drop) => Some(write.unwrap_or(0.0) + drop.unwrap_or(0.0)),
    };
    MoveRead {
        cost,
        size,
        drop_size,
        drop_cost,
        dropped_thinking_tokens,
    }
}

/// The shed rule's destination for `pin` among the eligible siblings, as its own move would read it: the no-return
/// keep, the allowance filter and the cap as `gate` applies them, the order `fits` alone ranked by the strategy with
/// the longest runway breaking its tie, as `rank` reads them, so the plan names the account the pin's own move lands
/// on when nothing changes before its next request, save a tie the runway leaves under Pace or Priority, where the
/// plan names the first tied candidate and the move draws one of them at random. `planned` holds the reservations the
/// round planned for its earlier marks, which the fit counts on their destinations. `None` when nothing fits.
#[allow(clippy::too_many_arguments)]
fn destination_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    eligible: &[&Candidate],
    pin: &PinRow,
    planned: &[Reservation],
    now: i64,
    fallback_ms: i64,
) -> Option<(String, FitRead)> {
    let window = window_for(&pin.model);
    let set: Vec<&Candidate> = eligible
        .iter()
        .copied()
        .filter(|c| c.account_id != pin.account_id)
        .filter(|c| !keeps_off(pin, &c.account_id, pool, now))
        .filter(|c| {
            window == SEVEN_DAY || holds_allowance(state, snapshot, &c.account_id, window, now)
        })
        .filter(|c| under_cap(state, pool, &c.account_id))
        .collect();
    let reads: Vec<(String, FitRead)> = set
        .iter()
        .map(|c| {
            (
                c.account_id.clone(),
                fit_of(
                    state,
                    snapshot,
                    pool,
                    request,
                    Figures::Pin(pin),
                    &c.account_id,
                    planned,
                    now,
                    Judge::Move,
                    None,
                ),
            )
        })
        .collect();
    let fits: Vec<&Candidate> = order(&set, &reads, FitWord::Fits)
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    let ranked = rank(
        state,
        snapshot,
        pool,
        window,
        &fits,
        &reads,
        &[],
        now,
        fallback_ms,
        None,
    )?;
    let chosen = ranked.planned.account_id.clone();
    reads.into_iter().find(|(id, _)| *id == chosen)
}

/// `destination_of`'s destination for `pin` with the `move_read` at it, or with none where it finds none.
#[allow(clippy::too_many_arguments)]
fn move_at(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    req: &SelectRequest,
    eligible: &[&Candidate],
    pin: &PinRow,
    planned: &[Reservation],
    now: i64,
    fallback_ms: i64,
) -> (Option<(String, FitRead)>, MoveRead) {
    let found = destination_of(
        state,
        snapshot,
        pool,
        req,
        eligible,
        pin,
        planned,
        now,
        fallback_ms,
    );
    let read = move_read(
        state,
        req,
        pin,
        now,
        found.as_ref().map(|(id, _)| id.as_str()),
    );
    (found, read)
}

/// The members of `pool` that no obstacle holds off for `model` at the lane speed `fast` names, with no account
/// attempted: the set `run` builds for a pin's own next request at its first attempt. No request stands behind a
/// standing, so the instant and the fallback are the read's own.
pub(crate) fn standing_eligible(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    model: &str,
    fast: bool,
    now: i64,
    fallback_ms: i64,
) -> Vec<Candidate> {
    snapshot
        .members(&pool.id)
        .iter()
        .filter(|member| {
            obstacle_on(
                state,
                snapshot,
                pool,
                &member.account_id,
                model,
                &[],
                now,
                fallback_ms,
                fast,
            )
            .is_none()
        })
        .map(|member| Candidate {
            account_id: member.account_id.clone(),
            incarnation: snapshot.incarnation(&member.account_id).unwrap_or(0),
            priority: member.priority,
            upstream_model: model.to_string(),
        })
        .collect()
}

/// A standing pin's `move_read` at the destination a round's order reads for it now among `eligible`, the set
/// `standing_eligible` answers for the pin's pool, model and lane speed, with no booking planned. `pool` is the pool
/// of the pin's key, `None` where it left the registry.
pub(crate) fn standing_move_read(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: Option<&PoolRow>,
    eligible: &[Candidate],
    pin: &PinRow,
    now: i64,
    fallback_ms: i64,
) -> MoveRead {
    let request = standing_request(pin);
    match pool {
        Some(pool) => {
            let eligible: Vec<&Candidate> = eligible.iter().collect();
            let (_, read) = move_at(
                state,
                snapshot,
                pool,
                &request,
                &eligible,
                pin,
                &[],
                now,
                fallback_ms,
            );
            read
        }
        // A pin whose pool left the registry has no siblings to plan a move among.
        None => move_read(state, &request, pin, now, None),
    }
}

/// A shedding round over the pool's pins on the account `current` sits on: every pin of it under one of the pool's
/// keys, ordered in a window-share round by the span's average share of the trigger's window the walk prices them by,
/// and in a `usd` round by the dollar burn at `now` (cold first, those whose move drops no thinking by burn and then
/// the rest by burn over the dropped thinking at the weight of output, then warm by burn over the rewrite plus that
/// weighted drop, each move read at the pin's own destination, bounded by `warm_sessions`, a fast-lane pin out of a
/// window-share round, and a pin whose model does not draw the trigger's window and a pin with no priced burn on it out
/// of any round, since none of them has a share of it to shed, a pin whose model draws a model week out of a round the
/// Fable reservation fired (`fable_model`), since its use is what the reservation keeps room for, and a pin whose agent
/// ended out of any round too, since its agent sends no new turn and marking its decayed burn relieves nothing), then
/// walked while the removed burn is under the target, `min(excess, shed_cap x burn_sum)` on the trigger's window, and
/// `shed_cap x burn_sum` under the reservation, whose test names no excess. Once one pin's burn covers what the
/// target still asks, the walk marks the covering pin whose move costs the least, `cheapest` on the record, rather than
/// the next in the order: a warm pin's rewrite plus the share of the thinking its move drops, and a cold pin's share of
/// that thinking alone. Where no covering pin has a destination, the walk marks the pin at its position only where its
/// burn covers `SHED_MARK_SHARE` of the rest, `sliver` otherwise. A standing mark is a move pending, so its burn counts
/// as removed and the pin is skipped. A pin whose mark would leave the account's other unmarked priced pins under
/// `SHED_MARK_SHARE` of the burn sum is never marked, `last_pin` before its gate is read, while no unpriced pin stands
/// unmarked. An unpriced pin is never marked, and a pin with no fitting destination counts nothing. Each mark plans a
/// reservation on its destination, which the next pin's fit counts, so a sibling's headroom is taken once. The current
/// pin's warmth and lane are this request's, every other pin's the stored ones.
#[allow(clippy::too_many_arguments)]
fn shed_round(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    eligible: &[&Candidate],
    current: &PinRow,
    current_cold: bool,
    current_fast: bool,
    now: i64,
    fallback_ms: i64,
    trigger: &Trigger,
) -> Round {
    let account_id = &current.account_id;
    let unit = burn_unit(state, snapshot, account_id, now, fallback_ms);
    let in_pool = in_pool(snapshot, pool);
    let average_ms = burn_average_ms(pool);
    // A window-share round orders by the walk's own figure, filled in below, so it lists its pins unpriced.
    let mut pins: Vec<(PinRow, Option<f64>)> = match unit {
        BurnUnit::WindowShare => state
            .pins_on(account_id, &in_pool)
            .into_iter()
            .map(|p| (p.clone(), None))
            .collect(),
        BurnUnit::Usd => {
            state
                .burn_of(account_id, &in_pool, unit, trigger.window, now, average_ms)
                .0
        }
    };
    // The pins whose agent ended join with no burn for the record's rows alone, each written under its word below.
    pins.extend(
        state
            .pins_on(account_id, &ended_in_pool(snapshot, pool))
            .into_iter()
            .map(|p| (p.clone(), None)),
    );
    let is_pin = |p: &PinRow, key: &PinKey| p.key_parts() == key.parts();
    let current_key = current.key();
    let is_current = |p: &PinRow| is_pin(p, &current_key);
    let is_cold = |p: &PinRow| {
        if is_current(p) {
            current_cold
        } else {
            !warm(p, now)
        }
    };
    // The only place a left-out pin's skip word is written, so the record and the strip read one spelling.
    let mut left_out: Vec<(PinRow, &'static str)> = Vec::new();
    let mut leave_out =
        |pins: Vec<(PinRow, Option<f64>)>, word: &'static str, out: &dyn Fn(&PinRow) -> bool| {
            let (left, kept): (Vec<_>, Vec<_>) = pins.into_iter().partition(|(p, _)| out(p));
            left_out.extend(left.into_iter().map(|(p, _)| (p, word)));
            kept
        };
    if unit == BurnUnit::WindowShare {
        let on_fast_lane = |p: &PinRow| {
            if is_current(p) {
                current_fast
            } else {
                lane_is_fast(&p.lane)
            }
        };
        pins = leave_out(pins, "fast_lane", &on_fast_lane);
    }
    // A pin whose model does not draw the trigger's window burns nothing there whatever the pricer answers, since
    // the projection that fired summed the burn of the drawing models alone: it has no share to shed and none the burn
    // guard counts as left unmarked, so it leaves before the order under both units. A fast-lane pin leaves first,
    // since it has no plan share at all.
    let off_window = |p: &PinRow| !plan_windows(&p.model).any(|window| window == trigger.window);
    pins = leave_out(pins, "off_window", &off_window);
    // A round the Fable reservation fired sheds the 7-day room that the account's Fable use is expected to need, so a
    // pin whose model draws a model week is that use and stays, though it draws the 7-day window too.
    if matches!(trigger.source, TriggerSource::FableReservation { .. }) {
        pins = leave_out(pins, "fable_model", &|p: &PinRow| {
            model_week(&p.model).is_some()
        });
    }
    // The keep left a pin whose agent ended out of every figure, so the round lists it for its record and walks it
    // nowhere, its pin and cache kept for a respawn. A fast-lane pin, a pin off the window and the Fable pin of a
    // reservation's round keep their words whatever their agent did.
    pins = leave_out(pins, "ended", &|p: &PinRow| p.ended);

    // The walk's figures: each pin's burn on the trigger's window in share per second, `None` for a pin the pricer
    // does not weight, the one case left once the partition above took the pins whose model does not draw the window,
    // and the record's figures per hour. The excess is the average rate to remove over `[now, horizon)` as the band
    // weighs it, so each pin's rate is its own average over that span under its regime's band: a rate at `now` would
    // over-read or under-read the pin's relief by the band's factor on a span the band crosses. The span is at least
    // the one second the excess divides by. Under the stretch the excess is the demand's, so each pin's rate reads over
    // the speed that demand was read at and the round removes what the slow-down cannot cover in the excess's own
    // units. Each pin's figure is computed once before the order, which reads it in a window-share round, and the
    // pick's loop reads every covering candidate at every position.
    let tier = snapshot.priced_tier(account_id).unwrap_or_default();
    let speed = trigger
        .speed
        .filter(|speed| matches!(trigger.source, TriggerSource::SlowDown { .. }) && *speed > 0.0)
        .unwrap_or(1.0);
    let horizon = trigger.horizon.max(now.saturating_add(1_000));
    // A walked pin beside its figures: `burn` the order sorts by, `share` the walk's figure, `read` its move at the
    // destination its own next request would take, and `destination` that account with its fit's runway.
    struct RoundPin {
        row: PinRow,
        burn: Option<f64>,
        share: Option<f64>,
        read: MoveRead,
        destination: Option<(String, Option<i64>)>,
    }
    let pins: Vec<(PinRow, Option<f64>, Option<f64>)> = pins
        .into_iter()
        .map(|(p, burn)| {
            let share = state
                .share()
                .rate(
                    trigger.window,
                    p.entrypoint.as_deref(),
                    tier,
                    &p.model,
                    &p.burn,
                    now,
                )
                .map(|rate| {
                    let segments = state.share().band_segments(
                        trigger.window,
                        p.entrypoint.as_deref(),
                        now,
                        horizon,
                    );
                    Banded::of(burn_decayed(rate, p.burn_at, now, average_ms), &segments)
                        .divided(speed)
                        .increase()
                        / seconds(horizon - now)
                });
            // A window-share round orders its pins by the figure the walk prices, so on a span the band crosses the
            // record lists the rows in the order of the figures it shows. A `usd` round keeps the dollar burn at
            // `now`, which counts the fast and classifier usage the account pays for beside the row's copy.
            let burn = if unit == BurnUnit::WindowShare {
                share
            } else {
                burn
            };
            (p, burn, share)
        })
        .collect();
    // A pin with no priced burn on the window has nothing to shed, so it leaves before the order and the burn guard as
    // an off-window pin does. An unpriced pin stays walked, since its burn is unknown rather than none.
    let (burnless, pins): (Vec<_>, Vec<_>) = pins
        .into_iter()
        .partition(|(_, _, share)| share.is_some_and(|share| share == 0.0));
    left_out.extend(burnless.into_iter().map(|(p, _, _)| (p, "no_burn")));
    // The order reads each walked pin's move where the pin's own next request would land it, with no booking planned,
    // since what a move rewrites and drops depends on the destination.
    let pins: Vec<RoundPin> = pins
        .into_iter()
        .map(|(p, burn, share)| {
            // The request's own row holds its sums as the caller handed them, which this selection writes, so the
            // order, the pick, the plan and the record read one row.
            let current_row = is_current(&p);
            let row = if current_row {
                PinRow {
                    thinking_sums: current.thinking_sums.clone(),
                    ..p
                }
            } else {
                p
            };
            let req = pin_request(request, &row, current_row);
            let (found, read) = move_at(
                state,
                snapshot,
                pool,
                &req,
                eligible,
                &row,
                &[],
                now,
                fallback_ms,
            );
            RoundPin {
                row,
                burn,
                share,
                read,
                destination: found.map(|(destination, fit)| (destination, fit.runway_ms)),
            }
        })
        .collect();
    let (mut cold, mut warm_pins): (Vec<_>, Vec<_>) =
        pins.into_iter().partition(|p| is_cold(&p.row));
    // A pin whose burn cannot be priced ranks after every priced pin of its group: a guess never orders a shed.
    fn priced_first<T>(
        a: &Option<T>,
        b: &Option<T>,
        order: impl Fn(&T, &T) -> Ordering,
    ) -> Ordering {
        match (a, b) {
            (Some(a), Some(b)) => order(a, b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
    // A pin with no prompt size reported yet ranks as the cheapest rewrite, at one token.
    let size = |p: &RoundPin| p.read.size.unwrap_or(1.0);
    let per_rewrite = |p: &RoundPin| p.burn.map(|b| b / size(p));
    let by_rewrite = |a: &RoundPin, b: &RoundPin| {
        priced_first(&per_rewrite(a), &per_rewrite(b), |x, y| y.total_cmp(x))
    };
    // A cold pin's cache is rewritten wherever it runs next, so the pins that drop nothing lead by burn, and those
    // whose move drops thinking, which the session pays again as output, follow by burn over the weighted drop.
    let by_drop = |p: &RoundPin| {
        p.burn.map(|burn| match p.read.drop_size {
            None => (false, burn),
            Some(drop) => (true, burn / drop),
        })
    };
    cold.sort_by(|a, b| {
        priced_first(&by_drop(a), &by_drop(b), |(a_drops, a), (b_drops, b)| {
            a_drops.cmp(b_drops).then(b.total_cmp(a))
        })
    });
    warm_pins.sort_by(by_rewrite);
    let cold_count = cold.len();
    let mut ordered = cold;
    ordered.extend(warm_pins);
    if pool.shedding.warm_sessions == WarmSessions::Always {
        ordered.sort_by(by_rewrite);
    }
    let per_hour = |share: f64| share * 3_600.0;
    let burn_sum: f64 = ordered.iter().filter_map(|p| p.share).sum();
    let cap = shed_cap(pool);
    let target = trigger
        .source
        .excess()
        .map_or(cap * burn_sum, |excess| excess.min(cap * burn_sum));
    let dwell_ms = shed_dwell_ms(pool);
    // The round never takes nearly all of an account's burn: a mark is refused where the other unmarked priced pins
    // would hold under `SHED_MARK_SHARE` of the round's burn sum. An unpriced pin's burn is unknown and not none, so
    // while one stands unmarked it is the pin that stays and the guard refuses nothing.
    let unpriced_stays = ordered
        .iter()
        .any(|p| p.share.is_none() && !mark_holds(&p.row));
    let leaves_too_little = |candidate: usize, marks: &[Mark]| {
        if unpriced_stays {
            return false;
        }
        let left: f64 = ordered
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                *i != candidate
                    && !mark_holds(&p.row)
                    && !marks.iter().any(|m| is_pin(&p.row, &m.key))
            })
            .filter_map(|(_, p)| p.share)
            .sum();
        left < SHED_MARK_SHARE * burn_sum
    };
    // A standing mark is a move pending, so its burn counts as removed before the walk marks anything else.
    let mut removed: f64 = ordered
        .iter()
        .filter(|p| mark_holds(&p.row))
        .filter_map(|p| p.share)
        .sum();
    let mut marks: Vec<Mark> = Vec::new();
    let mut planned: Vec<Reservation> = Vec::new();
    // By position in `ordered`: each pin's read and the destination its mark planned.
    let mut reads = vec![PinRead::default(); ordered.len()];
    let mut destinations: Vec<Option<String>> = vec![None; ordered.len()];
    let past_the_cold =
        |index: usize| pool.shedding.warm_sessions == WarmSessions::Never && index >= cold_count;
    // The destination the walked pin's own move would take beside the round's earlier marks, the figures the record
    // shows of it, and the booking a mark plans there: `no_runway` where nothing fits.
    let plan_of = |walked: &RoundPin, planned: &[Reservation]| {
        let p = &walked.row;
        let req = pin_request(request, p, is_current(p));
        // The round reads the state without writing it, so with nothing planned the order's read is the one a read
        // here would answer.
        let (found, moved) = if planned.is_empty() {
            (walked.destination.clone(), walked.read)
        } else {
            let (found, moved) = move_at(
                state,
                snapshot,
                pool,
                &req,
                eligible,
                p,
                planned,
                now,
                fallback_ms,
            );
            (
                found.map(|(destination, fit)| (destination, fit.runway_ms)),
                moved,
            )
        };
        let mut read = PinRead {
            moved: Some(moved),
            ..PinRead::default()
        };
        let Some((destination, runway_ms)) = found else {
            read.skip = Some("no_runway");
            return (read, None);
        };
        let destination_tier = snapshot.priced_tier(&destination).unwrap_or_default();
        let (rewrite, pin_burn, regime) = pin_figures(
            state,
            pool,
            &destination,
            &req,
            Figures::Pin(p),
            destination_tier,
            now,
        );
        read.runway_ms = runway_ms;
        let model = req.canonical_model.as_str();
        // The record shows the Pro-week figure of the write alone, one unit for every pin.
        let write = rewrite_write(state, &req, Some(p), Some(&destination));
        read.rewrite = state
            .share()
            .equivalent(SEVEN_DAY, req.entrypoint.as_deref(), model, write, now)
            .filter(|share| *share > 0.0);
        // The window names where the write's share was read, so it follows `rewrite` beside the cost: a pin whose write
        // prices nothing and whose drop is priced states neither.
        read.rewrite_window = read
            .rewrite
            .and(read.moved.and_then(|moved| moved.cost))
            .map(|_| window_for(model));
        let booking = Reservation {
            account_id: destination,
            attempt: request.attempt,
            pin: Some(p.key()),
            model: p.model.clone(),
            entrypoint: regime.map(String::from),
            booked_at: now,
            rewrite,
            burn: pin_burn,
            expires_at: now.saturating_add(IN_FLIGHT_RECLAIM_MS),
        };
        (read, Some(booking))
    };
    // A move's cost: a warm pin pays its rewrite, the Pro share of the week its model draws, plus the share of the
    // thinking its move drops, which the session pays again as output. A cold pin's cache is rewritten wherever it runs
    // next, the reason the order walks the cold pins first, so it pays the dropped thinking's share alone. Under
    // `always` the order ignores warmth, and every pin pays its rewrite and its drop. A pin with no prompt size
    // reported and no drop costs nothing, as the order ranks it. The record's Pro-week figure never orders a move,
    // since it reads a Fable pin's rewrite without its week's factor.
    let cost_of = |p: &PinRow, read: &PinRead| {
        let moved = read.moved.unwrap_or_default();
        if pool.shedding.warm_sessions == WarmSessions::Always || !is_cold(p) {
            moved.cost.unwrap_or(0.0)
        } else {
            moved.drop_cost.unwrap_or(0.0)
        }
    };
    let mut met = false;
    let mut cheapest: Option<usize> = None;
    for (index, walked) in ordered.iter().enumerate() {
        let p = &walked.row;
        // A covering pin the pick read at an earlier position keeps what the pick's loop found there.
        let mut read = std::mem::take(&mut reads[index]);
        if mark_holds(p) {
            read.skip = Some("standing");
        } else if met || removed >= target || past_the_cold(index) {
            // The target is met, or `never` stops after the cold pins: the rest of the order is left as it is.
        } else if let Some(burn) = walked.share {
            // The cheapest sufficient pin: a pin is indivisible, and for a rest under one pin's burn the order would
            // mark the pin that sheds the most per token, however far its burn overshoots the rest. Once a pin from
            // this position on covers the rest, the walk marks, among the covering pins the gate finds a destination
            // for, the one whose move costs the least, the first in the order among equals, and the target is met.
            let rest = target - removed;
            let mut pick: Option<(f64, usize, &PinRow, f64, Reservation)> = None;
            for (offset, covering) in ordered[index..].iter().enumerate() {
                let Some(covers) = covering.share.filter(|covers| *covers >= rest) else {
                    continue;
                };
                let q = &covering.row;
                if mark_holds(q) || past_the_cold(index + offset) {
                    continue;
                }
                // A pin the guard keeps gets no gate read, so a pin the round would never move never reads
                // `no_runway`, the word the attention list's shed condition reads.
                if leaves_too_little(index + offset, &marks) {
                    reads[index + offset] = PinRead {
                        skip: Some("last_pin"),
                        ..PinRead::default()
                    };
                    continue;
                }
                let (q_read, booking) = plan_of(covering, &planned);
                let cost = cost_of(q, &q_read);
                if let Some(booking) = booking {
                    if pick.as_ref().is_none_or(|(least, ..)| cost < *least) {
                        pick = Some((cost, index + offset, q, covers, booking));
                    }
                }
                reads[index + offset] = q_read;
            }
            read = std::mem::take(&mut reads[index]);
            if let Some((_, position, q, covers, booking)) = pick {
                marks.push(Mark { key: q.key() });
                destinations[position] = Some(booking.account_id.clone());
                removed += covers;
                planned.push(booking);
                cheapest = Some(position);
                met = true;
            } else if burn < SHED_MARK_SHARE * rest {
                // A pin under the floor relieves too little of the rest to be worth its move, warm or cold, so it
                // stays.
                read.skip = Some("sliver");
            } else if burn < rest && leaves_too_little(index, &marks) {
                read.skip = Some("last_pin");
            } else if burn < rest {
                // No covering pin has a destination, so the order marks this pin where its own gate finds one. A
                // covering pin here already reads what the pick's loop found, the guard's `last_pin` or its gate's
                // `no_runway`.
                let (own, booking) = plan_of(walked, &planned);
                read = own;
                if let Some(booking) = booking {
                    marks.push(Mark { key: p.key() });
                    destinations[index] = Some(booking.account_id.clone());
                    removed += burn;
                    planned.push(booking);
                }
            }
        } else {
            // An unpriced burn moves the removed sum nowhere, so marking the pin would shed a session on a guess: it
            // stays, and while it stands unmarked the burn guard refuses no mark.
            read.skip = Some("unpriced");
        }
        reads[index] = read;
    }
    let (outcome, reason) = if marks.is_empty() {
        // The deepest class in the order `no_runway`, `sliver`, `off_window`, `fable_model`, `ended`, `no_burn`,
        // `unpriced`, `last_pin`, `standing`: a walked pin's skip, or where no pin was walked a left-out pin's own
        // word. `unpriced` where neither names a class, since no priced burn was there to remove. `no_runway` leads,
        // the word the attention list's shed condition reads.
        let mut skips: Vec<&'static str> = reads.iter().filter_map(|read| read.skip).collect();
        if ordered.is_empty() {
            // `fast_lane` names no class of the list.
            skips.extend(left_out.iter().map(|(_, word)| *word));
        }
        let reason = [
            "no_runway",
            "sliver",
            "off_window",
            "fable_model",
            "ended",
            "no_burn",
            "unpriced",
            "last_pin",
            "standing",
        ]
        .into_iter()
        .find(|word| skips.contains(word))
        .unwrap_or("unpriced");
        ("nothing_to_mark".to_string(), Some(reason))
    } else {
        (format!("marked {}", marks.len()), None)
    };
    // A left-out pin's row (`fast_lane`, `off_window`, `fable_model`, `ended` or `no_burn`) carries no share and no
    // read, so its figures follow the partition's word and never what the pricer answers for it.
    let pin_facts = |p: &PinRow, walked: Option<(usize, &RoundPin)>, left: Option<&'static str>| {
        let position = walked.map(|(i, _)| i);
        let read = position.map(|i| &reads[i]);
        let burn = walked.and_then(|(_, w)| w.share).map(per_hour);
        let destination = position.and_then(|i| destinations[i].as_ref());
        ShedPinFacts {
            key_id: p.key_id.clone(),
            session_id: p.session_id.clone(),
            agent_id: p.agent_id.clone(),
            model: p.model.clone(),
            cold: is_cold(p),
            burn,
            last_input_tokens: p.last_input_tokens,
            // The pick's read where it read the pin, else the order's.
            dropped_thinking_tokens: read
                .and_then(|read| read.moved)
                .or(walked.map(|(_, w)| w.read))
                .and_then(|moved| moved.dropped_thinking_tokens),
            per_rewrite: walked.zip(burn).map(|((_, w), b)| b / size(w)),
            marked: destination.is_some() || mark_holds(p),
            rewrite: read.and_then(|read| read.rewrite),
            rewrite_window: read.and_then(|read| read.rewrite_window),
            planned: destination.cloned(),
            runway_ms: read.and_then(|read| read.runway_ms),
            skip: left.or_else(|| read.and_then(|read| read.skip)),
            note: if position.is_some_and(|i| cheapest == Some(i)) {
                Some("cheapest")
            } else {
                is_current(p).then_some("current")
            },
        }
    };
    let pins: Vec<ShedPinFacts> = ordered
        .iter()
        .enumerate()
        .map(|(i, p)| pin_facts(&p.row, Some((i, p)), None))
        .chain(
            left_out
                .iter()
                .map(|(p, word)| pin_facts(p, None, Some(*word))),
        )
        .collect();
    let skipped = pins
        .iter()
        .filter_map(|facts| {
            let key = PinKey::new(
                &facts.key_id,
                &facts.session_id,
                &facts.agent_id,
                &facts.model,
            );
            facts
                .skip
                .map(|skip| format!("{}: {skip}", pin_key_text(&key)))
        })
        .collect();
    // The record echoes the two settings in their own unit, seconds.
    let whole_seconds = |ms: i64| u64::try_from(ms / 1_000).unwrap_or(u64::MAX);
    let facts = ShedRoundFacts {
        window: trigger.window.to_string(),
        utilization: trigger.utilization,
        projection: trigger.projection,
        horizon: trigger.horizon,
        ceiling: trigger.ceiling,
        trigger: trigger.source.as_str(),
        stretch_ms: trigger.source.stretch_ms(),
        threshold_ms: trigger.source.threshold_ms(),
        fable_reservation: trigger.source.fable_reservation(),
        factor: trigger.factor,
        unit: unit.as_str(),
        rewrite_unit: "pro_week",
        burn_average_seconds: whole_seconds(average_ms),
        shed_cap: cap,
        burn_sum: per_hour(burn_sum),
        // The reservation's test names no rate, and every pin the round walked spends the reserved room.
        excess: per_hour(trigger.source.excess().unwrap_or(burn_sum)),
        target: per_hour(target),
        mark_share: SHED_MARK_SHARE,
        removed: per_hour(removed),
        warm_sessions: pool.shedding.warm_sessions.as_str(),
        pins,
        marks: marks.iter().map(|mark| pin_key_facts(&mark.key)).collect(),
        outcome,
        reason,
        dwell_seconds: whole_seconds(dwell_ms),
        dwell_until: now.saturating_add(dwell_ms),
    };
    Round {
        facts,
        marks,
        skipped,
    }
}

fn pin_key_facts(key: &PinKey) -> PinKeyFacts {
    PinKeyFacts {
        key_id: key.key_id.clone(),
        session_id: key.session_id.clone(),
        agent_id: key.agent_id.clone(),
        model: key.model.clone(),
    }
}

// The request's fields come one by one, so a caller that holds no request can judge an account as well.
#[allow(clippy::too_many_arguments)]
pub(crate) fn obstacle(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    blocking: Option<&CooldownRow>,
    attempted: &[String],
    now: i64,
    fallback_ms: i64,
    fast: bool,
) -> Option<Obstacle> {
    let tried = attempted.iter().any(|a| a == account_id);
    // A candidate the registry no longer holds was deleted after admission. It is nobody's to select, and a cooldown
    // row it left behind must not date an exhaustion no wait can clear.
    let Some(account) = snapshot.account(account_id) else {
        return Some(Obstacle::Deleted);
    };
    if !account.enabled {
        return Some(Obstacle::Paused);
    }
    if unready(state, account, now).is_some() {
        return Some(Obstacle::Unready);
    }
    if subscription_ended(snapshot, account_id, now) {
        return Some(Obstacle::Ended);
    }
    let cooldown = |row: &CooldownRow| Obstacle::Cooldown {
        reason: row.reason.clone(),
        until: row.until,
        window: row.window.clone(),
        scope: row.scope.clone(),
    };
    // A fast rate limit dates the fast lane alone: where the account's credits (their row or the poll's state) or
    // its budget block the request too, that blocker answers, since the fast reset would pass with the request
    // still refused.
    let fast_limited = blocking.filter(|row| row.scope == CooldownScope::Fast);
    if let Some(row) = blocking.filter(|_| fast_limited.is_none()) {
        return Some(cooldown(row));
    }
    if fast {
        if let Some(row) = fast_credits_row(state, account_id, now) {
            return Some(cooldown(row));
        }
        if let Some(reason) = state.credits_blocked(snapshot, account_id, now) {
            return Some(Obstacle::Credits {
                reason: reason.into(),
            });
        }
    }
    if budget_unknown(state, snapshot, &pool.id, account_id) {
        return Some(Obstacle::BudgetUnknown);
    }
    if let Some((period, until)) = budget_exhausted(state, snapshot, &pool.id, account_id, now) {
        // A period that ends inside the fast window would answer a retry the fast lane still refuses.
        return Some(Obstacle::Budget {
            until: fast_limited.map_or(until, |row| until.max(row.until)),
            period,
        });
    }
    if let Some(row) = fast_limited {
        return Some(cooldown(row));
    }
    // The spend funds every model and the fast lane alike, so a seat at its cap is out of the set for any request,
    // a live pin's renewal included, until the window resets.
    if let Some(reset) = spend_spent(state, snapshot, account_id, now) {
        return Some(Obstacle::Spent {
            until: reset,
            window: "spend".into(),
        });
    }
    // Plan overage is a plan window's state, which a fast request does not draw.
    if !fast && pool.overage_policy == OveragePolicy::Never {
        if let Some(deadline) = state
            .overage_deadline(account_id, fallback_ms)
            .filter(|d| *d > now)
        {
            return Some(Obstacle::Overage { until: deadline });
        }
    }
    // An account this request already tried is out of the set either way, so its cooldown is reported ahead of
    // the attempt: three accounts rate-limited inside one request answer with the earliest reset, not untimed.
    if tried {
        return Some(Obstacle::Attempted);
    }
    None
}

/// `obstacle` over the account's blocking cooldown for `model`, for a caller that reads the cooldown for nothing else.
#[allow(clippy::too_many_arguments)]
pub(crate) fn obstacle_on(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    model: &str,
    attempted: &[String],
    now: i64,
    fallback_ms: i64,
    fast: bool,
) -> Option<Obstacle> {
    obstacle(
        state,
        snapshot,
        pool,
        account_id,
        state.blocking_cooldown(account_id, model, now, fast),
        attempted,
        now,
        fallback_ms,
        fast,
    )
}

/// What keeps a feature call from going out under `account_id`, or the account's row where nothing does: `Deleted` for
/// an id the registry lacks, `Unready` for a stored state other than `ready`, `Ended` past the subscription's end,
/// tested in that order. A pause, a cooldown and a spent window stop model requests alone, so they answer the row here.
pub fn feature_ready<'a>(
    snapshot: &'a RegistrySnapshot,
    account_id: &str,
    now: i64,
) -> Result<&'a AccountRow, Obstacle> {
    let Some(account) = snapshot.account(account_id) else {
        return Err(Obstacle::Deleted);
    };
    if not_ready(account).is_some() {
        return Err(Obstacle::Unready);
    }
    if subscription_ended(snapshot, account_id, now) {
        return Err(Obstacle::Ended);
    }
    Ok(account)
}

/// The account's live fast-credits row, which refuses a fast request whatever later row stands beside it.
fn fast_credits_row<'a>(
    state: &'a RoutingState,
    account_id: &'a str,
    now: i64,
) -> Option<&'a CooldownRow> {
    state
        .cooldowns_of(account_id, now)
        .find(|row| row.scope == CooldownScope::FastCredits)
}

/// Renews the pin and persists the request's verdict on it: a refreshing request stamps its refresh and its
/// hashes, a cold one zeroes the refresh and stamps its hashes so the next continuation re-warms the pin, and
/// a foreign one leaves both alone. A request that carries no chain has no hashes to stamp, so the pin keeps
/// the last conversation it was judged against; its lane is stamped all the same, or a chainless body on a new
/// lane would leave the pin cold for good. `previous_landing` is the account step 1 read the session's previous
/// request landed on, which the selection names as `left` where it is another account than the pin's: a return to
/// the pin after a turn elsewhere refers to a message the pin's account cannot find.
#[allow(clippy::too_many_arguments)]
fn renew(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    current: &PinRow,
    key: &PinKey,
    candidate: &Candidate,
    request: &SelectRequest,
    (verdict, cause): (Warmth, WarmthCause),
    previous_landing: Option<&str>,
    writes: &mut Vec<StateWrite>,
    rec: &mut Recorder,
) -> Selection {
    let (now, fallback_ms) = (request.now, request.fallback_ms);
    let chained = request.static_hash().is_some() || !request.boundaries.is_empty();
    // The stored row is `current` itself: a move and a credit placed elsewhere take it off the state, and neither
    // reaches a renewal.
    let renewed = state
        .pin_mut(key)
        .expect("a renewed pin is the row its selection read from the state");
    renewed.last_used_at = now;
    match verdict {
        Warmth::Refresh => {
            renewed.last_refresh_at = now;
            renewed.auxiliary_only = false;
        }
        Warmth::Cold => renewed.last_refresh_at = 0,
        Warmth::Untouched => {}
    }
    if verdict != Warmth::Untouched {
        renewed.lane.clone_from(&request.lane);
        renewed.effort.clone_from(&request.effort);
        if chained {
            stamp_hashes(renewed, request);
        }
    }
    // A probe replays another request's headers and body, so the row keeps the entrypoint its own requests stamped.
    if !request.probe {
        renewed.entrypoint.clone_from(&request.entrypoint);
    }
    if zeroes_sums(request) {
        for sum in renewed.thinking_sums.values_mut() {
            sum.tokens = 0;
        }
    }
    let renewed = renewed.clone();
    let handed = pin_ref(&renewed);
    let (refresh_stamped, hashes_stamped) = (
        verdict == Warmth::Refresh,
        verdict != Warmth::Untouched && chained,
    );
    rec.renewal = Some(Renewal {
        cold: !warm_on(current, verdict, now),
        cause: cause.as_str(),
        epoch: current.epoch,
    });
    rec.step(
        "renew",
        "renewed",
        vec![
            account_fact(&current.account_id),
            Fact::text("lane", handed.lane.as_str()),
            Fact::bool("refresh_stamped", refresh_stamped),
            Fact::bool("hashes_stamped", hashes_stamped),
        ],
    );
    rec.pin_written = Some(PinWritten {
        action: "renewed",
        epoch: current.epoch,
        lane: handed.lane.clone(),
        refresh_stamped,
        hashes_stamped,
    });
    state.mark_renewed(key);
    writes.push(StateWrite::UpsertPin(renewed));
    // A bound or a count request, a portable one whose move found every destination at its cap, and one on a pool
    // with load shedding off, renew without the shed rule's read.
    if rec.trigger_windows.is_empty() {
        rec.trigger_windows = trigger_windows(state, snapshot, pool, &current.account_id);
    }
    let row_handed = handed_row(current, request.pin_ended, cause, now);
    Selection {
        account: selected_row(snapshot, &current.account_id),
        upstream_model: candidate.upstream_model.clone(),
        pin: Some(handed),
        migrated: None,
        in_overage: state.in_overage(&current.account_id, now, fallback_ms),
        shared_windows: shared_windows(state, &current.account_id),
        trace: SelectionTrace {
            strategy: pool.strategy.as_str(),
            renewed: true,
            cohort_hold: cohort_hold(&request.holds),
            ..SelectionTrace::default()
        },
        pool_id: pool.id.clone(),
        left: previous_landing
            .filter(|account| *account != current.account_id)
            .map(String::from),
        judged: None,
        previous_input_tokens: row_handed.map(|(size, _, _)| size),
        entry_expired: row_handed.map(|(_, expired, _)| expired),
        agent_released: row_handed.map(|(_, _, released)| released),
    }
}

/// The pin a selection hands out as the row it placed or renewed holds it.
fn pin_ref(row: &PinRow) -> PinRef {
    PinRef {
        key_id: row.key_id.clone(),
        session_id: row.session_id.clone(),
        agent_id: row.agent_id.clone(),
        model: row.model.clone(),
        epoch: row.epoch,
        lane: row.lane.clone(),
        entrypoint: row.entrypoint.clone(),
        effort: row.effort.clone(),
    }
}

/// Every path that selects an account has already passed it through `obstacle`, which answers `Deleted` for
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
        if candidate_in(&kept, &c.account_id).is_none() {
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
    snapshot: &RegistrySnapshot,
    allowed: &[&Candidate],
    window: &str,
    account_id: &str,
    now: i64,
) -> bool {
    candidate_in(allowed, account_id).is_some()
        && holds_allowance(state, snapshot, account_id, window, now)
}

/// The candidate of `set` for `account_id`, `None` for an account the set does not hold.
fn candidate_in<'a>(set: &[&'a Candidate], account_id: &str) -> Option<&'a Candidate> {
    set.iter().copied().find(|c| c.account_id == account_id)
}

/// What keeps placement off an affinity entry, `None` for one it may land on: `lane` (written on another lane than
/// the request's), `expired` (not live at the request's instant), `incarnation` (another holder of the id wrote it),
/// `not_admitted` (the account admits no fresh placement, which `admitted` answers as `admits` does). Shared by every
/// rung. Rung 4 adds its own blocker on top.
fn entry_refusal(
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    entry: &AffinityEntry,
    admitted: &dyn Fn(&str) -> bool,
) -> Option<&'static str> {
    if !same_lane(&entry.lane, &request.lane) {
        Some("lane")
    } else if !entry.live(request.now) {
        Some("expired")
    } else if !entry.held(|id| snapshot.incarnation(id)) {
        Some("incarnation")
    } else if !admitted(&entry.account_id) {
        Some("not_admitted")
    } else {
        None
    }
}

/// Rung 4's own gate on a candidate account, evaluated per candidate rather than inferred from the
/// post-partition selection set: under the cap, and a fit for the request (the moving `pin`'s figures, or the fresh
/// prior with none) as the `no_fit` partition reads it, whose first arm reads a winding-down account as `ending`.
/// Answers the first gate that blocks it: `at_cap` or `no_fit`. The fit leaves the row under `own` out, as the
/// partition's does. `verdict` is the fit the partition already read for the account, `None` to read it here. The cap
/// is read here even beside a `verdict`, since the relay moves the in-flight counts from other threads.
#[allow(clippy::too_many_arguments)]
fn blocked(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    figures: Figures<'_>,
    account_id: &str,
    own: Option<&PinKey>,
    verdict: Option<FitWord>,
) -> Option<&'static str> {
    let now = request.now;
    if !under_cap(state, pool, account_id) {
        Some("at_cap")
    } else if verdict.unwrap_or_else(|| {
        fit_of(
            state,
            snapshot,
            pool,
            request,
            figures,
            account_id,
            &[],
            now,
            Judge::Placement,
            own,
        )
        .verdict
    }) != FitWord::Fits
    {
        Some("no_fit")
    } else {
        None
    }
}

/// The read `reads` holds for `account_id`, `None` for an account it does not hold.
fn read_of<T: Copy>(reads: &[(&str, T)], account_id: &str) -> Option<T> {
    reads
        .iter()
        .find(|(id, _)| *id == account_id)
        .map(|(_, read)| *read)
}

/// Whether `account_id` holds a slot under the pool's in-flight cap.
fn under_cap(state: &RoutingState, pool: &PoolRow, account_id: &str) -> bool {
    state.in_flight().account(account_id) < pool.failover.in_flight_cap
}

/// Whether `account_id`'s subscription is inside the pool's wind-down before its end.
fn winding_down(snapshot: &RegistrySnapshot, pool: &PoolRow, account_id: &str, now: i64) -> bool {
    snapshot
        .term(account_id)
        .is_some_and(|term| term::winding_down(term, now, pool.failover.wind_down_minutes))
}

/// Whether `account_id`'s subscription has ended, as `term::ended` judges it.
fn subscription_ended(snapshot: &RegistrySnapshot, account_id: &str, now: i64) -> bool {
    snapshot
        .term(account_id)
        .is_some_and(|term| term::ended(term, now))
}

/// The candidate a fallback credit token places the request on: the issuer `credit_account` names, among the eligible
/// set under the incarnation that answered the refusal, and under its in-flight cap unless the live pin is already
/// there, since a renewal reads no cap. A count creates no pin and follows its pin, a probe warms the pin it was sent
/// for, and a non-portable request keeps its binding, so none of them reads a credit.
fn credit_candidate<'a>(
    request: &SelectRequest,
    eligible: &[&'a Candidate],
    pinned: Option<&str>,
    state: &RoutingState,
    pool: &PoolRow,
) -> Option<&'a Candidate> {
    let credit = request.credit_account.as_ref()?;
    if request.count || request.probe || !request.portable {
        return None;
    }
    let candidate = candidate_in(eligible, &credit.account_id)
        .filter(|c| c.incarnation == credit.incarnation)?;
    let renews = pinned == Some(candidate.account_id.as_str());
    (renews || under_cap(state, pool, &candidate.account_id)).then_some(candidate)
}

/// The words a placement directed ahead of the moves and the strategy writes: `reason` on the rungs, the pick and,
/// where a live pin stands, the move rules, `fact` as the key of the pin step's fact naming an account, the credit's
/// issuer or the judged turn's.
#[derive(Clone, Copy)]
struct DirectedWords {
    reason: &'static str,
    fact: &'static str,
}

/// The fallback credit token's placement, on the account that issued the token.
const CREDIT_WORDS: DirectedWords = DirectedWords {
    reason: "credit",
    fact: "credit",
};

/// The classifier call's placement on the account its judged turn ended on or the one that turn's pin stands on.
const JUDGED_WORDS: DirectedWords = DirectedWords {
    reason: "classifier",
    fact: "judged",
};

/// The `direction` of a classifier call neither of `judged_candidate`'s accounts took, placed by the rungs and the
/// strategy as a request without a pin is. A call a fallback credit directs reads no direction, since its `credit`
/// fact names where it went and no strategy ran.
const PLACED_DIRECTION: &str = "strategy";

/// The candidate the client's classifier call goes to, with the direction word that names it: the account and
/// incarnation the session's judged turn's stream ended on (`turn_account`), where the upstream keeps the call free,
/// else the account and incarnation that turn's pin stands on now (`pin_account`), each among the eligible set and
/// holding allowance, as the `with_main` rung admits a parent's account. In a session with no recorded turn end, as
/// after a restart, the pin's account stands in for the first. The cap does not bound it, as it bounds no renewal:
/// the call belongs to a session already on that account. A non-portable call takes the direction too: it holds no pin
/// to be bound to, and the judged turn's account is where a reference of the conversation resolves. `None` for every
/// other request, for a count and a probe, and where neither account can serve the call, which is then placed by the
/// rungs and the strategy. The call holds no pin, so a call directed here moves none: the conversation's next turn
/// makes the move. `turn` is the judged turn `decide` read ahead of the run.
fn judged_candidate<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    turn: Option<&JudgedTurn>,
    eligible: &[&'a Candidate],
    window: &str,
) -> Option<(&'a Candidate, &'static str)> {
    if !request.classifier || request.count || request.probe {
        return None;
    }
    let turn = turn?;
    let serves = |account_id: &str, incarnation: u64| {
        candidate_in(eligible, account_id)
            .filter(|c| c.incarnation == incarnation)
            .filter(|c| holds_allowance(state, snapshot, &c.account_id, window, request.now))
    };
    turn.end
        .as_ref()
        .and_then(|(account_id, incarnation)| serves(account_id, *incarnation))
        .map(|c| (c, "turn_account"))
        .or_else(|| {
            turn.pin
                .as_ref()
                .and_then(|(_, account_id, incarnation)| serves(account_id, *incarnation))
                .map(|c| (c, "pin_account"))
        })
}

/// A classifier call's judged turn, read once ahead of the run: the account and incarnation of the session's recorded
/// stream end (`latest_turn_end`), and the pin `latest_turn_pin` answers with the account and incarnation it stands
/// on.
struct JudgedTurn {
    end: Option<(String, u64)>,
    pin: Option<(PinRef, String, u64)>,
}

/// A fresh request with no live pin lands where its prefix is already warm: the deepest message boundary
/// (rung 3, always, bypassing the cap as a pin renewal does, since a tail match is the conversation's own
/// account); else, behind the class knob, the parent agent's pin (`with_main`, bypassing the cap the same way,
/// since the cohort's own account outweighs it) or the deepest static boundary past the size gate, the cache
/// gate and the blocker (rung 4, already under the cap by construction). `allowed` is the selection set before
/// step 6's own partitions, so the source of a move or an account a preference narrowed around can never win a
/// placement it would not have won on its own account. `moving` is the pin a move takes off its source, whose figures
/// the blocker's fit reads, and `None` for a fresh placement. A token count is never placed: it reads no cache, so a
/// warm prefix would only lift it past the cap and promise its trace a read it can never make. `own` is the request's
/// pin key, whose row the blocker's fit leaves out. `prior` is the fresh prior step 6 built, empty for a move.
/// `allowances` and `verdicts` are step 6's allowance and fit reads by account, which the rungs reuse where they hold
/// the account: the state is the same, and step 6 reads the fit of a fresh placement alone.
#[allow(clippy::too_many_arguments)]
fn placement_of<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    moving: Option<&PinRow>,
    prior: &[PinRow],
    affinity: &AffinityMap,
    allowed: &[&'a Candidate],
    window: &str,
    own: Option<&PinKey>,
    allowances: &[(&str, bool)],
    verdicts: &[(&str, FitWord)],
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
    // One verdict per allowed account across the walks, since `state` stays borrowed immutably for the whole
    // placement and an account's allowance and its rung 4 blocker read the same at every entry of it.
    let index_of = |account_id: &str| allowed.iter().position(|c| c.account_id == account_id);
    let admitted_at: Vec<OnceCell<bool>> = allowed
        .iter()
        .map(|c| read_of(allowances, &c.account_id).map_or_else(OnceCell::new, OnceCell::from))
        .collect();
    let admitted = |account_id: &str| {
        index_of(account_id).is_some_and(|i| {
            *admitted_at[i]
                .get_or_init(|| holds_allowance(state, snapshot, account_id, window, now))
        })
    };

    let content = affinity.walk(&request.canonical_model, &request.boundaries, |e| {
        entry_refusal(snapshot, request, e, &admitted)
    });
    record_lookups(rec, &content, now, true);
    let matched = content.matched.map(|i| &content.visited[i]);
    if let Some((visited, candidate)) =
        matched.and_then(|v| candidate_in(allowed, &v.entry.account_id).map(|c| (v, c)))
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

    let is_subagent = request.agent_id != MAIN_AGENT;
    if !is_subagent {
        none(rec, "rung:with_main", "class");
    } else if pool.affinity.subagent_placement != SubagentPlacement::WithMain {
        none(rec, "rung:with_main", "policy_off");
    } else {
        // The class knob places a subagent with its parent or not at all: the static rung never runs under it.
        let parent = state.pin(&PinKey {
            agent_id: request
                .parent_agent_id
                .clone()
                .unwrap_or_else(|| MAIN_AGENT.to_string()),
            ..request.pin_key()
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
        } else if !admits(state, snapshot, allowed, window, &pin.account_id, now) {
            Some("not_admitted")
        } else {
            None
        };
        let placed = match refusal {
            None => candidate_in(allowed, &pin.account_id),
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
    let blocked_at: Vec<OnceCell<Option<&'static str>>> =
        allowed.iter().map(|_| OnceCell::new()).collect();
    let walk = affinity.walk(&request.canonical_model, &request.static_boundaries, |e| {
        entry_refusal(snapshot, request, e, &admitted)
            .or_else(|| (!size_gate(e)).then_some("size_gate"))
            .or_else(|| {
                let judge = || {
                    blocked(
                        state,
                        snapshot,
                        pool,
                        request,
                        Figures::of(moving, prior),
                        &e.account_id,
                        own,
                        read_of(verdicts, &e.account_id),
                    )
                };
                index_of(&e.account_id).map_or_else(judge, |i| *blocked_at[i].get_or_init(judge))
            })
    });
    record_lookups(rec, &walk, now, false);
    let Some(visited) = walk.matched.map(|i| &walk.visited[i]) else {
        none(rec, "rung:static", "no_entry");
        return None;
    };
    let Some(candidate) = candidate_in(allowed, &visited.entry.account_id) else {
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
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    allowed: &[&'a Candidate],
    window: &str,
    idle_ms: i64,
    rec: &mut Recorder,
) -> Option<&'a Candidate> {
    let session_id = request.session_id.as_str();
    let account_id = state
        .session_pins(&request.key_id, session_id, &request.canonical_model)
        .find(|p| {
            request.now - p.last_used_at <= idle_ms
                && admits(state, snapshot, allowed, window, &p.account_id, request.now)
        })?
        .account_id
        .clone();
    let found = candidate_in(allowed, &account_id);
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
    if request.agent_id == MAIN_AGENT
        || pool.affinity.subagent_placement != SubagentPlacement::ByCohort
        || (!request.marker && request.portable)
    {
        return None;
    }
    let (_, entry) =
        affinity.deepest(&request.canonical_model, &request.static_boundaries, |e| {
            e.prefix_tokens > 0
                && entry_refusal(snapshot, request, e, &|id| {
                    admits(state, snapshot, allowed, window, id, request.now)
                })
                .is_none()
        })?;
    let found = candidate_in(allowed, &entry.account_id);
    if found.is_some() {
        rec.hold_place = Some("cohort");
    }
    found
}

const WEEK_HOURS: f64 = (WEEK_MS / HOUR_MS) as f64;

/// Pace's remaining allowance for a candidate at `now`, and the predicted reading's reset when it came from the
/// candidate's own `window` (an api_key account's worst-bucket reset never enters the score). A subscription
/// window nobody has read, an api_key account with every bucket refunded or aged, a usage-based seat, which holds
/// neither, or an account never sampled, all read as untouched: remaining 1, no reset. A reading past its reset
/// reads as the router's own use since it.
fn remaining(state: &RoutingState, account_id: &str, window: &str, now: i64) -> (f64, Option<i64>) {
    if let Some(reading) = state.predicted(account_id, window, now) {
        return ((1.0 - reading.utilization).max(0.0), reading.reset);
    }
    match state.sample(account_id).and_then(|s| s.worst_bucket(now)) {
        Some(reading) => ((1.0 - reading.utilization).max(0.0), None),
        None => (1.0, None),
    }
}

/// Pace's inputs and score for a candidate at `now`: the spare rate, the remaining allowance over the hours to the
/// earlier of the window's reset and the account's end less the demand of the pool's pins on the account on
/// that window (their burn over the account's realized speed), in share per hour. An account that runs out sooner is
/// spent first, and one whose sessions already spend its allowance ranks below an idle one. The score reads negative
/// where the account spends faster than its pace allows. An untiered account prices no burn. A paid source scores by
/// its remaining share alone: an api_key account by its worst live bucket (a bucket refills within a minute, so Pace
/// degrades to least utilized on it, and neither the bucket's own reset nor a burn enters the score), a usage-based
/// seat, which reads no plan window, as untouched. On a `seven_day` request the score holds back the member's Fable
/// reservation (`fable_reservation`) from the remaining allowance, unfloored, so a member whose reservation exceeds its
/// room scores under one with room left, and the burn it subtracts is then that of the pins and bookings whose model
/// draws no model week, since the Fable ones stand in the reservation. `choose` ranks by this score and the record
/// carries the rest, so the two cannot disagree. `own` is the request's pin key on a placement's own score, whose row
/// the demand leaves out as the placement's fit does, and `None` on every other read.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pace_of(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    incarnation: u64,
    window: &str,
    now: i64,
    own: Option<&PinKey>,
) -> PaceFacts {
    pace_figures(
        state,
        snapshot,
        pool,
        account_id,
        incarnation,
        window,
        now,
        own,
        true,
    )
}

/// `pace_of`'s figures, with the Fable reservation where `reserve` is set and the score and burn of no reservation
/// otherwise.
#[allow(clippy::too_many_arguments)]
fn pace_figures(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    id: &str,
    incarnation: u64,
    window: &str,
    now: i64,
    own: Option<&PinKey>,
    reserve: bool,
) -> PaceFacts {
    let (remaining, reset) = remaining(state, id, window, now);
    if paid_source(snapshot, id) {
        return PaceFacts {
            remaining,
            reset,
            end: None,
            deadline: None,
            hours: None,
            score: remaining,
            window: None,
            burn: 0.0,
            fable_reservation: None,
        };
    }
    let end = account_end(snapshot, id, now);
    let deadline = reset.into_iter().chain(end).min();
    let hours = deadline
        .map(|d| ((d - now) as f64 / HOUR_MS as f64).max(1.0 / 60.0))
        .unwrap_or(WEEK_HOURS);
    let keep = in_pool_without(snapshot, pool, own);
    let tier = snapshot.priced_tier(id);
    let per_hour = |shares: &BTreeMap<String, f64>, window: &str| {
        shares.get(window).copied().unwrap_or(0.0) * 3_600.0
    };
    // A slowed account's calm is its holds' work, so the score reads what its sessions would spend unslowed.
    let speed = || state.slow_down_speed(pool, id, now);
    // A reservation applies on `seven_day` alone, so the pass there also sums the burn the score subtracts beside one.
    let (demand, off_model_weeks) = match tier {
        Some(tier) if reserve && window == SEVEN_DAY => {
            let (demand, off_model_weeks) = state.demand_shares_and_off_model_weeks(
                id,
                &keep,
                tier,
                now,
                burn_average_ms(pool),
                speed(),
            );
            (demand, Some(off_model_weeks))
        }
        Some(tier) => (
            state.demand_shares(id, &keep, tier, now, burn_average_ms(pool), speed()),
            None,
        ),
        None => (BTreeMap::new(), None),
    };
    let fable_reservation = if reserve {
        fable_reservation_of(
            state,
            id,
            incarnation,
            window,
            now,
            hours,
            per_hour(&demand, SEVEN_DAY_FABLE),
        )
    } else {
        None
    };
    let burn = match (fable_reservation, &off_model_weeks) {
        (Some(_), Some(off_model_weeks)) => per_hour(off_model_weeks, window),
        _ => per_hour(&demand, window),
    };
    PaceFacts {
        remaining,
        reset,
        end,
        deadline,
        hours: Some(hours),
        score: (remaining - fable_reservation.unwrap_or(0.0)) / hours - burn,
        window: Some(window.to_string()),
        burn,
        fable_reservation,
    }
}

/// The share of a member's 7-day room Pace holds back on a `seven_day` request for the member's expected use of the
/// Fable week until the score's deadline: the pricer's `week_factor` times the smaller of the Fable room and the
/// stronger of two rates over the score's `hours`, the peak hour of the member's record among the clock hours that
/// ended within as many hours back as the score looks ahead (the hour still running counts at its sum so far) and
/// `live`, the pins' demand on the week, each in share of the week per hour. `None` on a request on a model week, on a
/// member with no Fable reading and where the pricer answers no factor.
fn fable_reservation_of(
    state: &RoutingState,
    id: &str,
    incarnation: u64,
    window: &str,
    now: i64,
    hours: f64,
    live: f64,
) -> Option<f64> {
    if window != SEVEN_DAY {
        return None;
    }
    let reading = state.predicted(id, SEVEN_DAY_FABLE, now)?;
    let factor = state.share().week_factor(SEVEN_DAY_FABLE, now)?;
    let room = (1.0 - reading.utilization).max(0.0);
    let since = now as f64 - hours * HOUR_MS as f64;
    let peak = state.week_peak(id, incarnation, SEVEN_DAY_FABLE, since);
    Some(factor * room.min(peak.max(live) * hours))
}

/// `choose`'s sort key for a candidate: under the kind order, group one is oauth with plan quota, two oauth in
/// overage, three a paid source (an api_key account or a usage-based seat). The split holds under any
/// overage_policy, because it orders by cost, not eligibility: `allow` still demotes an overage oauth account behind
/// a plan-quota one. `overage_policy` governs eligibility everywhere and orders the tier itself only when the kind
/// order is off, through the overage bit. The kind order puts the group ahead of the priority. Within a group,
/// `prefer_plan` still orders an overage account behind a plan-quota one, group three (the paid sources) included. A
/// candidate step 6 kept on extra usage with a spent window (`spent_model`) bills as an account in overage does, so
/// every plan-funded sibling goes ahead of it whatever the priorities: it joins the overage group under the kind order
/// and leads the key otherwise. The third element is the overage bit under either order, which the selection trace
/// reads.
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
    if pool.prefer_subscription {
        let group = if paid_source(snapshot, &candidate.account_id) {
            2
        } else {
            u8::from(in_overage || spent)
        };
        (group, candidate.priority, overage_bit)
    } else {
        (u8::from(spent), candidate.priority, overage_bit)
    }
}

/// The strategy's reading of a set before the draw: the tier `tier_of` ranks first, its members, their Pace scores
/// with the Pace facts each was read from (the burn it subtracts in share per hour and the Fable reservation it holds
/// back) where the strategy reads them, the candidates the strategy draws
/// among (the best Pace score, the whole tier under Priority, the tier in id order under round robin), the ties the
/// longest runway kept where it narrowed them (empty otherwise, and always under round robin, where the cursor picks),
/// and the one a round plans on: the first of the ties the runway left, or under round robin the one at the cursor's
/// turn, read without passing the turn on. `choose` draws from `by_runway`, else from `ties`, and records the pick, so
/// a plan and the move it foresees read one rule.
struct Ranked<'a> {
    tier: Tier,
    members: Vec<&'a Candidate>,
    scores: Vec<Score>,
    paces: Vec<PaceFacts>,
    ties: Vec<&'a Candidate>,
    by_runway: Vec<&'a Candidate>,
    planned: &'a Candidate,
}

/// `reads` holds the fit read of the candidates whose runway breaks the strategy's tie, found by account id. `own` is
/// the request's pin key on a placement's ranking, which `pace_of` reads as the placement's own score.
#[allow(clippy::too_many_arguments)]
fn rank<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    set: &[&'a Candidate],
    reads: &[(String, FitRead)],
    spent_model: &[String],
    now: i64,
    fallback_ms: i64,
    own: Option<&PinKey>,
) -> Option<Ranked<'a>> {
    let keyed: Vec<(&'a Candidate, Tier)> = set
        .iter()
        .map(|c| {
            (
                *c,
                tier_of(state, snapshot, pool, c, spent_model, now, fallback_ms),
            )
        })
        .collect();
    let tier = keyed.iter().map(|(_, key)| *key).min()?;
    let members: Vec<&'a Candidate> = keyed
        .iter()
        .filter(|(_, key)| *key == tier)
        .map(|(c, _)| *c)
        .collect();
    let (scores, paces, ties, turn) = match pool.strategy {
        Strategy::Pace => {
            let paces: Vec<PaceFacts> = members
                .iter()
                .map(|c| {
                    pace_of(
                        state,
                        snapshot,
                        pool,
                        &c.account_id,
                        c.incarnation,
                        window,
                        now,
                        own,
                    )
                })
                .collect();
            let scores: Vec<Score> = members
                .iter()
                .zip(&paces)
                .map(|(c, pace)| Score {
                    account_id: c.account_id.clone(),
                    score: pace.score,
                    unit: score_unit(snapshot, &c.account_id),
                })
                .collect();
            let best = scores
                .iter()
                .map(|s| s.score)
                .fold(f64::NEG_INFINITY, f64::max);
            let ties = members
                .iter()
                .copied()
                .zip(&scores)
                .filter(|(_, s)| s.score == best)
                .map(|(c, _)| c)
                .collect();
            (scores, paces, ties, 0)
        }
        Strategy::Priority => (Vec::new(), Vec::new(), members.clone(), 0),
        Strategy::RoundRobin => {
            // The cursor walks the tier's sorted ids, so an account's turn does not depend on the order
            // `resolve` happened to build the candidate list in.
            let mut cohort = members.clone();
            cohort.sort_by(|a, b| a.account_id.cmp(&b.account_id));
            let ids: Vec<&str> = cohort.iter().map(|c| c.account_id.as_str()).collect();
            let turn = state.round_robin_turn(&pool.id, tier, &ids);
            (Vec::new(), Vec::new(), cohort, turn)
        }
    };
    let by_runway = match pool.strategy {
        Strategy::Pace | Strategy::Priority => runway_ties(&ties, reads),
        Strategy::RoundRobin => Vec::new(),
    };
    let planned = match by_runway.first() {
        Some(first) => *first,
        None => ties.get(turn).copied()?,
    };
    Some(Ranked {
        tier,
        members,
        scores,
        paces,
        ties,
        by_runway,
        planned,
    })
}

/// The ties whose runway is the longest among them, an unbounded one longest of all as `longest` reads it. A tie with
/// no read keeps its place, since nothing says its runway is shorter. Empty when the runway narrows nothing, so the
/// pick line names `by_runway` only where it broke the tie.
fn runway_ties<'a>(ties: &[&'a Candidate], reads: &[(String, FitRead)]) -> Vec<&'a Candidate> {
    let runway = |c: &Candidate| {
        reads
            .iter()
            .find(|(account_id, _)| *account_id == c.account_id)
            .map(|(_, read)| read.rank_runway_ms.unwrap_or(i64::MAX))
    };
    let Some(best) = ties.iter().filter_map(|c| runway(c)).max() else {
        return Vec::new();
    };
    let kept: Vec<&'a Candidate> = ties
        .iter()
        .copied()
        .filter(|c| runway(c).is_none_or(|ms| ms == best))
        .collect();
    if kept.len() == ties.len() {
        Vec::new()
    } else {
        kept
    }
}

/// `places` is false for a request the cap holds: the choice names the account it waits for and places nothing.
/// `reads` holds the fit reads whose runway breaks the strategy's tie, empty where none is to break it.
/// `spent_model` names the candidates step 6 kept on extra usage with a spent window. The pick's line names the tier,
/// every tier member's score and burn under Pace with its Fable reservation where one applies, the strategy's ties,
/// the ones the runway kept where it narrowed them, whether a draw was left to make and the round-robin turn, and the
/// tier's members take their rank, the chosen one first, and under Pace the Pace facts they ranked by. `own` is the
/// request's pin key, whose row the Pace scores leave out.
#[allow(clippy::too_many_arguments)]
fn choose<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    set: &[&'a Candidate],
    reads: &[(String, FitRead)],
    spent_model: &[String],
    now: i64,
    fallback_ms: i64,
    places: bool,
    own: Option<&PinKey>,
    rng: &mut impl Rng,
    rec: &mut Recorder,
) -> Option<(&'a Candidate, Tier)> {
    let strategy = pool.strategy.as_str();
    let Some(Ranked {
        tier: first,
        members: tier,
        scores,
        paces,
        ties,
        by_runway,
        ..
    }) = rank(
        state,
        snapshot,
        pool,
        window,
        set,
        reads,
        spent_model,
        now,
        fallback_ms,
        own,
    )
    else {
        rec.step("pick", "none", vec![Fact::text("strategy", strategy)]);
        return None;
    };
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
    let drawn_from: &[&Candidate] = if by_runway.is_empty() {
        &ties
    } else {
        &by_runway
    };
    let draw_facts = |facts: &mut Vec<Fact>| {
        let ids = |set: &[&Candidate]| set.iter().map(|c| c.account_id.clone()).collect();
        facts.push(Fact::list("ties", ids(&ties)));
        if !by_runway.is_empty() {
            facts.push(Fact::list("by_runway", ids(&by_runway)));
        }
        facts.push(Fact::bool("drawn", drawn_from.len() > 1));
    };
    let chosen = match pool.strategy {
        Strategy::Pace => {
            for (score, pace) in scores.iter().zip(&paces) {
                facts.push(
                    Fact::number("score", score.score)
                        .unit(score.unit)
                        .about(&score.account_id),
                );
                facts.push(
                    Fact::number("burn", pace.burn)
                        .unit("percent_per_hour")
                        .about(&score.account_id),
                );
                if let Some(reservation) = pace.fable_reservation {
                    facts.push(
                        Fact::number("fable_reservation", reservation)
                            .unit("percent")
                            .about(&score.account_id),
                    );
                }
                // The step-2 read of a candidate holds no reservation, since the first tier is known here alone, so a
                // member of it takes the figures it ranked by.
                if let Some(member) = rec.candidate_mut(&score.account_id) {
                    member.pace = Some(pace.clone());
                }
            }
            draw_facts(&mut facts);
            Some(drawn_from[rng.random_range(0..drawn_from.len())])
        }
        Strategy::Priority => {
            draw_facts(&mut facts);
            Some(drawn_from[rng.random_range(0..drawn_from.len())])
        }
        Strategy::RoundRobin => {
            let cohort: Vec<&str> = ties.iter().map(|c| c.account_id.as_str()).collect();
            let i = state.round_robin_next(&pool.id, first, &cohort, places);
            facts.push(Fact::text("turn", format!("{} of {}", i + 1, cohort.len())));
            facts.push(Fact::bool("drawn", false));
            ties.get(i).copied()
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
            Obstacle::Cooldown {
                scope: CooldownScope::Fast,
                until,
                ..
            } => Some(*until),
            _ => None,
        })
        .min();
    if let Some(deadline) = fast_reset {
        return Exhaustion::FastLane { deadline };
    }
    let credits: Vec<Option<&str>> = obstacles
        .iter()
        .filter_map(|o| match o {
            Obstacle::Cooldown { scope, window, .. } => {
                Some((*scope == CooldownScope::FastCredits).then_some(window.as_str()))
            }
            Obstacle::Credits { reason } => Some(Some(reason.as_str())),
            Obstacle::Budget { .. }
            | Obstacle::BudgetUnknown
            | Obstacle::Overage { .. }
            | Obstacle::Spent { .. } => Some(None),
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
            Obstacle::Cooldown {
                until,
                reason,
                scope: CooldownScope::FastCredits,
                ..
            } => Some((until, "overage".into(), reason)),
            Obstacle::Cooldown {
                until,
                window,
                reason,
                ..
            } => Some((until, window, reason)),
            Obstacle::Overage { until } => Some((until, "overage".into(), "overage".into())),
            Obstacle::Spent { until, window } => Some((until, window, "spent".into())),
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
    kind.map_or("unknown", AccountKind::as_str)
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

/// Pace scores a subscription account per hour of its window and a paid source by its remaining share alone.
fn score_unit(snapshot: &RegistrySnapshot, account_id: &str) -> &'static str {
    if paid_source(snapshot, account_id) {
        "percent"
    } else {
        "percent_per_hour"
    }
}

fn pin_key_text(key: &PinKey) -> String {
    format!(
        "{}/{}/{}/{}",
        key.key_id, key.session_id, key.agent_id, key.model
    )
}

/// An obstacle as the exhausted line names it beside its account.
fn weighed_name(account_id: &str, obstacle: &Obstacle) -> String {
    format!("{account_id}:{}", obstacle.name())
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
            deadline: obstacle.until(),
            obstacle: Some(obstacle.as_str()),
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
fn pin_facts(
    found: &'static str,
    row: Option<&PinRow>,
    marked: bool,
    now: i64,
    pace: Option<&PinPace>,
) -> PinFacts {
    let mark = row.and_then(|p| p.mark.as_ref());
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
        shed: row.and_then(|p| p.shed.as_ref()).map(|shed| PinShed {
            from: shed.from.clone(),
            at: shed.at,
            reason: shed
                .reason
                .clone()
                .unwrap_or_else(|| SHED_ROUND.to_string()),
        }),
        marked,
        warmth: None,
        warmth_cause: None,
        released: Vec::new(),
        marked_at: mark.map(|m| m.at),
        marked_on: mark.map(|m| m.on.clone()),
        marked_window: mark.map(|m| m.window.clone()),
        cycle_ms: pace
            .and_then(|p| p.cycle_ms)
            .map(|cycle| cycle.round() as i64),
        slow_down_until: pace.and_then(|p| p.standing_deadline(now)),
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

fn window_facts(
    reading: &WindowReading,
    ahead: Option<f64>,
    projection: Option<&Projection>,
    reserved: f64,
    now: i64,
) -> WindowFacts {
    WindowFacts {
        utilization: reading.utilization,
        reset: reading.reset,
        source: reading.source.as_str(),
        observed_at: reading.observed_at,
        age_ms: now - reading.observed_at,
        ahead,
        projection: projection.map(|p| p.at_horizon),
        horizon: projection.map(|p| p.horizon),
        demand: None,
        reserved,
    }
}

fn obstacle_facts(obstacle: &Obstacle) -> ObstacleFacts {
    let none = ObstacleFacts {
        name: obstacle.name(),
        reason: None,
        scope: None,
        window: None,
        until: None,
    };
    match obstacle {
        Obstacle::Cooldown {
            reason,
            until,
            window,
            scope,
        } => ObstacleFacts {
            reason: Some(reason.clone()),
            scope: Some(scope.to_string()),
            window: Some(window.clone()),
            until: Some(*until),
            ..none
        },
        Obstacle::Budget { until, period } => ObstacleFacts {
            reason: Some(period.as_str().to_string()),
            until: Some(*until),
            ..none
        },
        Obstacle::Credits { reason } => ObstacleFacts {
            reason: Some(reason.clone()),
            ..none
        },
        Obstacle::Overage { until } => ObstacleFacts {
            reason: Some("overage".to_string()),
            window: Some("overage".to_string()),
            until: Some(*until),
            ..none
        },
        Obstacle::Spent { until, window } => ObstacleFacts {
            reason: Some("spent".to_string()),
            window: Some(window.clone()),
            until: Some(*until),
            ..none
        },
        Obstacle::Attempted
        | Obstacle::BudgetUnknown
        | Obstacle::Deleted
        | Obstacle::Ended
        | Obstacle::Paused
        | Obstacle::Unready => none,
    }
}

/// A member's standing as step 2 reads it, eligible or not: the readings the rules and the pick below weigh. The
/// tier is the one step 2 can know, with no member kept on extra usage yet. The partition corrects it. `own` is the
/// request's pin key where its Pace figures are the placement's own, as `choose` reads them, and `None` otherwise.
/// Beside the facts, the round's limit on each window they carry a projection of.
#[allow(clippy::too_many_arguments)]
fn candidate_facts(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    candidate: &Candidate,
    window: &str,
    blocking: Option<&CooldownRow>,
    obstacle: Option<&Obstacle>,
    own: Option<&PinKey>,
) -> (CandidateFacts, BTreeMap<&'static str, f64>) {
    let (now, id) = (request.now, candidate.account_id.as_str());
    let account = snapshot.account(id);
    let kind = account.map(|a| a.kind);
    let sample = state.sample(id);
    let mut windows = BTreeMap::new();
    let mut limits = BTreeMap::new();
    let mut burn = BTreeMap::new();
    // A usage-based seat draws no plan window, so its facts list none, its spend window drawn below.
    if kind != Some(AccountKind::ApiKey) && !snapshot.usage_based(id) {
        let projections = projections_of(
            state,
            snapshot,
            pool,
            id,
            snapshot.priced_tier(id).unwrap_or_default(),
            now,
            BurnRead::Achieved,
            None,
        );
        if let Some(tier) = snapshot.priced_tier(id) {
            burn = state
                .burn_shares(
                    id,
                    &in_pool(snapshot, pool),
                    tier,
                    now,
                    burn_average_ms(pool),
                )
                .into_iter()
                .map(|(window, share)| (window, share * 3_600.0))
                .collect();
        }
        let end = account_end(snapshot, id, now);
        for name in all_plan_windows() {
            if let Some(reading) = state.predicted(id, name, now) {
                // The shared windows alone carry a lead. A model week's reading names none.
                let ahead = (name == FIVE_HOUR || name == SEVEN_DAY)
                    .then(|| reading.utilization - elapsed_share_at(name, reading.reset, end, now));
                let mut facts = window_facts(
                    &reading,
                    ahead,
                    projections.get(name),
                    state.reserved(id, name),
                    now,
                );
                if name == FIVE_HOUR && kind == Some(AccountKind::Oauth) {
                    facts.demand = Some(five_hour_demand(state, snapshot, pool, id, &burn, now));
                }
                windows.insert(name, facts);
                if let Some(projection) = projections.get(name) {
                    limits.insert(name, projection.limit);
                }
            }
        }
    }
    // Drawn while a share is held: the entry's share is a figure, so a lapsed or unread share leaves no entry.
    if let Some((spend, utilization)) = state
        .spend_window(snapshot, id, now)
        .and_then(|spend| spend.utilization.map(|u| (spend, u)))
    {
        let observed_at = spend.observed_at.unwrap_or(now);
        windows.insert(
            "spend",
            WindowFacts {
                utilization,
                reset: Some(spend.reset),
                // A poll names no reset, so a share and a reset share a reading only where one head named both, and
                // the reset's source then reads `head` as the share's does: the reset's source answers every case.
                source: spend.reset_source.as_str(),
                observed_at,
                age_ms: now - observed_at,
                ahead: None,
                projection: None,
                horizon: None,
                demand: None,
                reserved: 0.0,
            },
        );
    }
    if let Some(reading) = sample.and_then(|s| s.worst_bucket(now)) {
        windows.insert("worst_bucket", window_facts(&reading, None, None, 0.0, now));
    }
    for name in [FAST_INPUT, FAST_OUTPUT] {
        if let Some(reading) = sample.and_then(|s| s.reading(name)) {
            windows.insert(name, window_facts(&reading, None, None, 0.0, now));
        }
    }
    let budget = budget_facts(state, snapshot, &pool.id, id);
    let mut cooldowns: Vec<CooldownFacts> = state
        .cooldowns_of(id, now)
        .map(|row| CooldownFacts {
            scope: row.scope.to_string(),
            reason: row.reason.clone(),
            window: row.window.clone(),
            until: row.until,
            blocking: blocking.is_some_and(|b| b.scope == row.scope),
        })
        .collect();
    cooldowns.sort_by(|a, b| a.scope.cmp(&b.scope));
    let credits = request
        .fast
        .then(|| state.holder_credits(snapshot, id))
        .flatten()
        .map(|c| CreditsFacts {
            enabled: c.enabled,
            disabled_reason: c.disabled_reason.clone(),
            utilization: c.utilization,
            observed_at: c.observed_at,
            blocked: state.credits_blocked(snapshot, id, now).map(String::from),
        });
    let term = snapshot.term(id).map(|t| {
        let (canceled, ends_at) = t.cancellation.stored_pair();
        TermFacts {
            since: t.since,
            trial_ends_at: t.trial_ends_at,
            canceled,
            ends_at,
            winding_down: term::winding_down(t, now, pool.failover.wind_down_minutes),
            ended: term::ended(t, now),
        }
    });
    let facts = CandidateFacts {
        account_id: id.to_string(),
        incarnation: candidate.incarnation,
        kind: kind_name(kind),
        plan_tier: snapshot.plan(id).map(String::from),
        enabled: account.is_some_and(|a| a.enabled),
        state: account.map_or("deleted", |a| a.state.as_str()),
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
        // No reservation outside the first tier, whose members `choose` gives the figures they ranked by.
        pace: (pool.strategy == Strategy::Pace).then(|| {
            pace_figures(
                state,
                snapshot,
                pool,
                id,
                candidate.incarnation,
                window,
                now,
                own,
                false,
            )
        }),
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
        runway_ms: None,
        fit: None,
        binding: None,
        burn,
        factor: state
            .slow_down_in_force(pool, id, now)
            .map(|entry| entry.factor),
    };
    (facts, limits)
}

/// A subscription member's demand on the 5-hour window, share per hour, from the entry standing at step 2: the achieved
/// burn the candidate table already read where the speed is 1, else the pins' burn over the speed with the bookings'
/// undivided.
fn five_hour_demand(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    burn: &BTreeMap<String, f64>,
    now: i64,
) -> f64 {
    let speed = state.slow_down_speed(pool, account_id, now);
    let demand = match snapshot.priced_tier(account_id) {
        Some(tier) if speed != 1.0 => state
            .demand_shares(
                account_id,
                &in_pool(snapshot, pool),
                tier,
                now,
                burn_average_ms(pool),
                speed,
            )
            .get(FIVE_HOUR)
            .map(|share| share * 3_600.0),
        _ => burn.get(FIVE_HOUR).copied(),
    };
    demand.unwrap_or(0.0)
}

fn request_facts(request: &SelectRequest, pool_id: String) -> RequestFacts {
    RequestFacts {
        received_at: request.received_at,
        body_tokens: Some(request.body_tokens),
        ..RequestFacts::of(RefusedRequest::from(request), pool_id, request.revision)
    }
}

/// The record and its summary from the answer and what the steps recorded: the outcome, every member's verdict,
/// and the row's `by` and `fact`, with the shedding round the decision ran, which the record keeps the marks of.
fn finish(
    rec: Recorder,
    request: &SelectRequest,
    pool_id: &str,
    pool: Option<&PoolRow>,
    result: &SelectResult,
) -> (DecisionRecord, DecisionSummary, Option<ShedRoundRecord>) {
    let Recorder {
        walk,
        mut candidates,
        pin,
        pin_reason,
        released,
        rule,
        source,
        moved,
        destinations,
        pin_written,
        hold_place,
        shed_round,
        trigger_windows,
        horizon_bounds,
        mark_cleared,
        pick,
        renewal,
        reservation,
        slow_down,
        hold_plan,
        direction: _,
        directed,
    } = rec;
    let settings = pool.map(settings_of);
    let pin = pin.map(|pin| PinFacts { released, ..pin });
    let (moved_word, moved_from) = moved.unzip();
    let mut outcome = Outcome {
        kind: "placed",
        account_id: None,
        from_account_id: None,
        upstream_model: None,
        tier: None,
        strategy: pool.map_or("", |p| p.strategy.as_str()),
        rule: rule.map(MigrateReason::as_str).or(moved_word),
        rung: None,
        in_overage: false,
        pin_written,
        pin_events: Vec::new(),
        hold: None,
        exhaustion: None,
        refusal: None,
        cohort_hold: cohort_hold(&request.holds),
        shed_marks: shed_round.as_ref().map(|round| round.facts.marks.clone()),
        mark_cleared,
        reservation,
        cause: None,
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
            outcome.from_account_id = source.clone().or(moved_from);
            outcome.upstream_model = Some(selection.upstream_model.clone());
            outcome.tier = selection.trace.tier;
            outcome.strategy = selection.trace.strategy;
            outcome.rung = selection.trace.placement.clone();
            outcome.in_overage = selection.in_overage;
            outcome.cohort_hold = selection.trace.cohort_hold.clone();
            // The decision that lands a failover's session on a pin names its cause. A held or exhausted one lands
            // nothing, and neither does a token count, which writes no pin.
            outcome.cause = request
                .failed_over
                .as_ref()
                .filter(|_| selection.pin.is_some())
                .and_then(|failed_over| failed_over.cause.clone());
            skipped = u32::try_from(selection.trace.skipped.len()).unwrap_or(u32::MAX);
        }
        SelectResult::Held(hold) => {
            outcome.kind = "held";
            outcome.account_id = Some(hold.account_id.clone());
            // The pick's tier is the hold's only when the hold waits on the pick's account, whatever placed the
            // hold there, as `scores` keys the pick's score.
            outcome.tier = pick
                .as_ref()
                .filter(|pick| pick.account_id == hold.account_id)
                .map(|pick| pick.tier);
            // A slow-down hold copies the read it was answered from, so the outcome states what the engine read.
            let slowed = hold.reason == HoldReason::SlowDown;
            let read = slow_down.as_ref().filter(|_| slowed);
            outcome.hold = Some(HoldOutcome {
                account_id: hold.account_id.clone(),
                cohort: hold.cohort.clone(),
                bound_ms: pool.map_or(0, |p| match hold.reason {
                    HoldReason::SlowDown | HoldReason::Rewrite => slow_down_hold_ms(p),
                    HoldReason::Reset | HoldReason::Cap => {
                        seconds_ms(p.failover.retry_in_place_seconds)
                    }
                }),
                reason: hold.reason,
                until: hold.until,
                factor: read.map(|read| read.ramped),
                speed: read.map(|read| read.realized),
                stretch_ms: read.map(|read| read.stretch_ms),
                threshold_ms: read.map(|read| read.threshold_ms),
                cycle_ms: hold_plan
                    .as_ref()
                    .filter(|_| slowed)
                    .and_then(|plan| plan.cycle_ms),
                waits_for: hold.waits_for.clone(),
            });
        }
        SelectResult::Exhausted(exhaustion) => {
            outcome.kind = "exhausted";
            outcome.exhaustion = Some(exhaustion_facts(exhaustion));
        }
        SelectResult::Refused(refusal) => {
            outcome.kind = "refused";
            outcome.refusal = Some(refusal.facts());
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
        candidates: &candidates,
        walk: &walk,
        pin: pin.as_ref(),
        renewal: renewal.as_ref(),
        hold_place,
        pick: pick.as_ref(),
        directed,
        trigger_windows: &trigger_windows,
        horizon_bounds: &horizon_bounds,
        exhaustion: match result {
            SelectResult::Exhausted(exhaustion) => Some(exhaustion),
            _ => None,
        },
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
    let record = DecisionRecord {
        version: RECORD_VERSION,
        request: request_facts(request, pool_id.to_string()),
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
    candidates: &'r [CandidateFacts],
    walk: &'r [WalkStep],
    pin: Option<&'r PinFacts>,
    renewal: Option<&'r Renewal>,
    hold_place: Option<&'static str>,
    pick: Option<&'r Pick>,
    directed: Option<DirectedWords>,
    exhaustion: Option<&'r Exhaustion>,
    trigger_windows: &'r [&'static str],
    horizon_bounds: &'r [BTreeMap<&'static str, f64>],
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
            // The wait explains the renewal more than the pin's idle age does: the row names the slow-down or the
            // rewrite in flight it followed and how long it lasted.
            "renewed" if self.held_wait().is_some() => {
                let hold = self.held_wait().expect("the guard read a wait");
                (by("hold", hold.reason.as_str()), vec![held_fact(hold)])
            }
            "renewed" => (by("none", ""), self.renewal_fact()),
            "moved" => {
                // A move continues as a fresh placement: its By says why it left, its Fact the runway of the
                // destination's row (the gate's on a rule's move, the runway to 1.0 on a `least_bad` landing), then
                // where it landed, or the verdict in words when no candidate fit.
                let mut facts = Vec::new();
                if let Some(member) = outcome.account_id.as_deref().and_then(|id| self.member(id)) {
                    facts.push(match member.runway_ms {
                        None => summary_fact("runway", FactValue::Text("unbounded".into()), "text"),
                        Some(0) => summary_fact("runway", FactValue::Text("none".into()), "text"),
                        Some(ms) => summary_fact("runway", FactValue::Int(ms), "ms"),
                    });
                }
                // The fired rule's table is the walk's last: an earlier rule whose gate read a set and found nothing
                // left a table of its own before it.
                let order = self
                    .walk
                    .iter()
                    .rev()
                    .find(|w| w.step == "fit")
                    .map(|w| w.verdict);
                match order {
                    Some("least_bad") => facts.push(summary_fact(
                        "fit",
                        FactValue::Text("least bad".into()),
                        "text",
                    )),
                    Some("rewrite_only") => facts.push(summary_fact(
                        "fit",
                        FactValue::Text("rewrite only".into()),
                        "text",
                    )),
                    _ => facts.extend(self.placement_fact()),
                }
                (by("rule", outcome.rule.unwrap_or_default()), facts)
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
                    // A reset wait, a slow-down hold and a wait for a rewrite in flight are named by their reason,
                    // as the renewal names the slow-down, so `strategy_pick` stays the cap wait's word.
                    let place = match hold.reason {
                        HoldReason::Reset | HoldReason::Rewrite | HoldReason::SlowDown => {
                            hold.reason.as_str()
                        }
                        HoldReason::Cap if hold.cohort.is_some() => "cohort",
                        HoldReason::Cap => "strategy_pick",
                    };
                    (by("hold", place), vec![held_fact(hold)])
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
            "held" => match outcome.hold.as_ref() {
                // A wait for a reset names the wait's end, since no slot and no pick explain it. The end is the later
                // of the reset and a blocking cooldown's end, so the fact is named for the wait: the walk's
                // `move:spent` line keeps the reset itself. A moved pin's wait for the head of the request that
                // carried the move names its bound the same way.
                Some(hold) if matches!(hold.reason, HoldReason::Reset | HoldReason::Rewrite) => (
                    by("hold", hold.reason.as_str()),
                    hold.until
                        .map(|until| summary_fact("until", FactValue::Int(until), "time"))
                        .into_iter()
                        .collect(),
                ),
                // A slowed pin's wait names its end and the factor it was sized under: no slot, pick or reset explains
                // it.
                Some(hold) if hold.reason == HoldReason::SlowDown => {
                    (by("hold", hold.reason.as_str()), self.slow_down_fact())
                }
                _ => (
                    by("hold", self.hold_place.unwrap_or("strategy_pick")),
                    self.in_flight_fact(),
                ),
            },
            "exhausted" => {
                let Some(exhaustion) = &outcome.exhaustion else {
                    return (by("exhaustion", ""), Vec::new());
                };
                let run_out = self.run_out_hold();
                let mut facts: Vec<SummaryFact> = run_out
                    .or_else(|| self.attempt_hold())
                    .map(held_fact)
                    .into_iter()
                    .collect();
                let fact = match (&exhaustion.reason, exhaustion.deadline) {
                    (Some(reason), _) => {
                        summary_fact("cause", FactValue::Text(reason.clone()), "text")
                    }
                    (None, Some(deadline)) => {
                        summary_fact("retry", FactValue::Int(deadline), "time")
                    }
                    (None, None) => summary_fact("retry", FactValue::Text("none".into()), "text"),
                };
                facts.push(fact);
                // A request held at the cap was answered at the cap unless a leg was dialed. The relay's own reading
                // decides, since a freed slot another request took leaves no hold that ran out.
                if let Some(answered) = self.exhaustion.filter(|_| self.request.held_out) {
                    facts.push(answer_fact(exhaustion_status(
                        answered,
                        !self.request.dialed,
                    )));
                }
                (by("exhaustion", exhaustion.kind), facts)
            }
            _ => {
                let kind = outcome.refusal.as_ref().map_or("", |r| r.kind);
                (by("refusal", kind), vec![key_fact(&self.request.key_id)])
            }
        }
    }

    /// A probe's renewal names the pin's epoch. Any other names the warmth's cause when the pin was cold, else the
    /// utilization of the first plan window, in the trigger's order, whose projection at the horizon reads over its
    /// round's limit, else the pin's idle age.
    fn renewal_fact(&self) -> Vec<SummaryFact> {
        let Some(renewal) = self.renewal else {
            return Vec::new();
        };
        let cause = || {
            vec![summary_fact(
                "cause",
                FactValue::Text(renewal.cause.into()),
                "text",
            )]
        };
        if self.request.probe {
            let epoch = i64::try_from(renewal.epoch).unwrap_or(i64::MAX);
            return vec![summary_fact("epoch", FactValue::Int(epoch), "count")];
        }
        if renewal.cold {
            return cause();
        }
        let member = self
            .outcome
            .account_id
            .as_deref()
            .and_then(|id| self.member(id));
        if let Some(member) = member {
            // The windows the round's trigger read, in its order, the first over its round's limit named by its
            // utilization: the trigger's own comparison, strict, so the row never names a window no round would
            // fire on.
            let limits = self
                .candidates
                .iter()
                .position(|c| c.account_id == member.account_id)
                .and_then(|i| self.horizon_bounds.get(i));
            for name in self.trigger_windows.iter().copied() {
                let limit = limits.and_then(|limits| limits.get(name)).copied();
                if let Some(window) = member.windows.get(name).filter(|w| {
                    w.projection
                        .zip(limit)
                        .is_some_and(|(projection, limit)| projection > limit)
                }) {
                    return vec![summary_fact(
                        name,
                        FactValue::Number(window.utilization),
                        "percent",
                    )];
                }
            }
        }
        self.pin
            .and_then(|p| p.idle_ms)
            .map(|idle| vec![summary_fact("idle", FactValue::Int(idle), "ms")])
            .unwrap_or_else(cause)
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
        // The credit or the judged turn placed the request ahead of the strategy, which ranked no candidate.
        if let Some(words) = self.directed {
            return vec![text("pick", words.reason.into())];
        }
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

    /// The hold this selection follows: the request's last hold, when it held at the selection just before this
    /// one. A leg that failed over after the wait, on the wire or before it, took a selection of its own and reads
    /// `None`, whether or not the wire attempt moved. Every arm that names a wait reads it here.
    fn attempt_hold(&self) -> Option<&HoldFacts> {
        self.request
            .holds
            .last()
            .filter(|hold| hold.seq.checked_add(1) == Some(self.request.seq))
    }

    /// The slow-down hold or the wait for a rewrite in flight this selection follows, as `attempt_hold` reads it.
    fn held_wait(&self) -> Option<&HoldFacts> {
        self.attempt_hold()
            .filter(|hold| matches!(hold.reason, HoldReason::Rewrite | HoldReason::SlowDown))
    }

    /// A slow-down hold's `until` and the factor its read sized it under, a bare fraction as the walk states it.
    fn slow_down_fact(&self) -> Vec<SummaryFact> {
        let Some(hold) = self.outcome.hold.as_ref() else {
            return Vec::new();
        };
        let until = hold
            .until
            .map(|until| summary_fact("until", FactValue::Int(until), "time"));
        let factor = hold.factor.map(|factor| SummaryFact {
            key: "factor",
            value: FactValue::Number(factor),
            unit: None,
            entity: None,
        });
        until.into_iter().chain(factor).collect()
    }

    /// The request's last hold whose bound ran out, wherever it sits: a pool of several members at their cap passes
    /// the rest over with no wait of their own, so the exhaustion it led to can follow several selections later.
    fn run_out_hold(&self) -> Option<&HoldFacts> {
        self.request.holds.iter().rev().find(|hold| hold.ran_out)
    }

    /// A placement on the session's pin: `in_flight` as a hold's, then `held` with the wait when `attempt_hold`
    /// answers one.
    fn session_pin_fact(&self) -> Vec<SummaryFact> {
        let mut fact = self.in_flight_fact();
        if let Some(hold) = self.attempt_hold() {
            fact.push(held_fact(hold));
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

/// The `held` summary fact of the wait `hold` records, in milliseconds. Every arm that names a wait builds it here.
fn held_fact(hold: &HoldFacts) -> SummaryFact {
    summary_fact(
        "held",
        FactValue::Int(i64::try_from(hold.held_ms).unwrap_or(i64::MAX)),
        "ms",
    )
}

/// The `answer` summary fact: the HTTP status the client was answered with, a bare number with no unit.
fn answer_fact(status: u16) -> SummaryFact {
    SummaryFact {
        key: "answer",
        value: FactValue::Int(i64::from(status)),
        unit: None,
        entity: None,
    }
}

#[cfg(test)]
mod elapsed_share_tests {
    use super::{elapsed_share_at, FIVE_HOUR, SEVEN_DAY, WEEK_MS};

    const AT: i64 = 1_700_000_000_000;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn an_end_inside_the_window_steepens_the_share_toward_it() {
        // A tenth of the week gone, the end a tenth ahead: half of the span to the end has elapsed.
        let share = elapsed_share_at(
            SEVEN_DAY,
            Some(AT + WEEK_MS * 9 / 10),
            Some(AT + WEEK_MS / 10),
            AT,
        );
        assert!(close(share, 0.5), "{share}");
    }

    #[test]
    fn an_end_at_or_past_the_reset_reads_the_reset_alone() {
        let reset = Some(AT + WEEK_MS * 9 / 10);
        let plain = elapsed_share_at(SEVEN_DAY, reset, None, AT);
        assert!(close(plain, 0.1), "{plain}");
        assert_eq!(elapsed_share_at(SEVEN_DAY, reset, reset, AT), plain);
        assert_eq!(
            elapsed_share_at(SEVEN_DAY, reset, Some(AT + WEEK_MS * 3), AT),
            plain
        );
    }

    #[test]
    fn without_a_reset_nothing_has_elapsed_whatever_the_end() {
        assert_eq!(elapsed_share_at(FIVE_HOUR, None, None, AT), 0.0);
        assert_eq!(
            elapsed_share_at(FIVE_HOUR, None, Some(AT + 60_000), AT),
            0.0
        );
    }

    #[test]
    fn an_end_at_or_before_the_window_s_start_reads_fully_elapsed() {
        let reset = Some(AT + WEEK_MS / 2);
        let start = AT - WEEK_MS / 2;
        assert_eq!(elapsed_share_at(SEVEN_DAY, reset, Some(start), AT), 1.0);
        assert_eq!(
            elapsed_share_at(SEVEN_DAY, reset, Some(start - WEEK_MS), AT),
            1.0
        );
    }

    #[test]
    fn an_end_already_passed_inside_the_window_reads_fully_elapsed() {
        let share = elapsed_share_at(FIVE_HOUR, Some(AT + 3_600_000), Some(AT - 3_600_000), AT);
        assert_eq!(share, 1.0);
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

#[cfg(test)]
mod banded_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use phoebis_store::{
        AccountKind, AccountRow, Cancellation, PoolMember, PoolRow, RegistryLoad, RoutingLoad,
        SubscriptionTerm,
    };

    use super::{fit_of, lane_key, runway, slow_down_of, Banded, Figures, Judge, SelectRequest};
    use crate::state::{
        BandSegment, Reservation, RoutingState, SharePricer, UtilizationSample, WindowReading,
        WindowSource, FIVE_HOUR,
    };
    use crate::RegistrySnapshot;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    /// Two hours cut at the first, the first at `weights[0]` and the second at `weights[1]`.
    fn two_hours(weights: [f64; 2]) -> [BandSegment; 2] {
        [
            BandSegment {
                from: 0,
                to: 3_600_000,
                weight: weights[0],
            },
            BandSegment {
                from: 3_600_000,
                to: 7_200_000,
                weight: weights[1],
            },
        ]
    }

    #[test]
    fn banded_rates_weigh_each_piece_and_add_across_pins() {
        let rate = 1e-6;
        let entering = Banded::of(rate, &two_hours([1.0, 2.0]));
        assert!(
            close(entering.increase(), 3_600.0 * rate * 3.0),
            "a rate read outside the band is the plain one, which the band's hour doubles"
        );
        let leaving = Banded::of(rate, &two_hours([2.0, 1.0]));
        assert!(
            close(leaving.increase(), 3_600.0 * rate * 1.5),
            "a rate read inside the band carries its weight, so the plain rate after it is half"
        );
        let whole = [BandSegment {
            from: 0,
            to: 7_200_000,
            weight: 1.0,
        }];
        let sum = entering.plus(&Banded::of(rate, &whole));
        assert!(
            close(sum.increase_until(3_600_000), 3_600.0 * 2.0 * rate)
                && close(sum.increase(), 3_600.0 * 5.0 * rate),
            "a pin at one rate adds its rate on both pieces"
        );
        assert_eq!(
            entering.time_to(3_600.0 * rate),
            Some(3_600_000),
            "the room the first hour fills is reached at its edge"
        );
        assert_eq!(
            entering.time_to(3_600.0 * rate * 5.0),
            Some(10_800_000),
            "a room past the span is reached at the last piece's rate"
        );
        assert_eq!(entering.time_to(0.0), Some(0));
        assert_eq!(Banded::of(0.0, &two_hours([1.0, 2.0])).time_to(rate), None);
        assert!(
            close(
                Banded::read_at(rate, 2.0, &two_hours([1.0, 2.0])).increase(),
                entering.increase() / 2.0
            ),
            "a rate read inside the band and weighed from outside it reads half the plain rate"
        );
        assert!(close(
            entering.divided(2.0).increase(),
            entering.increase() / 2.0
        ));
        assert_eq!(Banded::flat(rate, 0, 7_200_000), Banded::of(rate, &whole));
    }

    #[test]
    fn a_runway_is_unbounded_where_it_reaches_the_earliest_wall_after_now() {
        const NOW: i64 = 1_000_000;
        // 0.01 at 0.8 an hour puts the limit 45 seconds away.
        let burn = Banded::flat(0.8 / 3_600.0, NOW, NOW + 3_600_000);
        let read =
            |reset: Option<i64>, end: Option<i64>| runway(0.94, &burn, 0.95, NOW, reset, end);
        assert_eq!(
            [
                read(Some(NOW + 45_000), None),
                read(Some(NOW + 45_001), None),
                read(Some(NOW), None),
                read(Some(NOW + 30_000), Some(NOW)),
            ],
            [None, Some(45_000), Some(45_000), None],
            "a reset on the runway's last millisecond, one a millisecond later, a reset at now, an end at now beside a \
             reset inside the runway"
        );
    }

    #[test]
    fn a_runway_reads_the_band_weighted_time_where_the_band_changes_inside_it() {
        // The first hour fills 0.36 of the 0.72 left, and the band's second hour at twice the rate fills the rest in
        // half an hour.
        let burn = Banded::of(1e-4, &two_hours([1.0, 2.0]));
        assert_eq!(runway(0.2, &burn, 0.92, 0, None, None), Some(5_400_000));
    }

    #[test]
    fn with_no_burn_a_runway_reads_zero_at_the_limit_and_unbounded_under_it() {
        let idle = Banded::default();
        assert_eq!(
            (
                runway(0.95, &idle, 0.95, 0, None, None),
                runway(0.5, &idle, 0.95, 0, None, None)
            ),
            (Some(0), None),
            "an idle window at its limit is spent, and one under it is never reached"
        );
    }

    #[test]
    fn two_planned_reservations_of_two_regimes_weigh_the_band_each_under_its_own() {
        const HOUR: i64 = 3_600_000;
        const T: i64 = 10 * HOUR;
        let (band_from, band_to) = (T + HOUR / 2, T + 10 * HOUR);
        let snapshot = RegistrySnapshot::from_load(
            RegistryLoad {
                accounts: vec![AccountRow::for_tests("b", AccountKind::Oauth)],
                members: vec![PoolMember {
                    pool_id: "p".into(),
                    account_id: "b".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                }],
                pools: vec![PoolRow::for_tests("p")],
                terms: vec![SubscriptionTerm {
                    account_id: "b".into(),
                    since: None,
                    trial_ends_at: None,
                    cancellation: Cancellation::Off,
                    tier: Some("default_claude_max_20x".into()),
                    usage_based: false,
                    plan: Some("max_20x".into()),
                    plan_ended_at: None,
                }],
                ..RegistryLoad::default()
            },
            [0; 32],
            0,
            1,
        );
        let mut state = RoutingState::from_load(RoutingLoad::default());
        // The case's own band: twice the weight inside it on the 5-hour window under the base regime, one segment at
        // weight 1 under `desk`.
        state.set_share(SharePricer::default().with_band_segments(
            move |window, entrypoint, from, to| {
                let weight = |at: i64| {
                    if band_from <= at && at < band_to {
                        2.0
                    } else {
                        1.0
                    }
                };
                if window != FIVE_HOUR || entrypoint == Some("desk") {
                    return vec![BandSegment {
                        from,
                        to,
                        weight: 1.0,
                    }];
                }
                let mut edges = vec![from];
                edges.extend(
                    [band_from, band_to]
                        .into_iter()
                        .filter(|e| from < *e && *e < to),
                );
                edges.push(to);
                edges
                    .windows(2)
                    .map(|pair| BandSegment {
                        from: pair[0],
                        to: pair[1],
                        weight: weight(pair[0]),
                    })
                    .collect()
            },
        ));
        state.record_sample(
            "b",
            UtilizationSample {
                observed_at: T,
                ..Default::default()
            }
            .with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.3,
                    reset: Some(T + HOUR),
                    observed_at: T,
                    source: WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
            T,
        );
        let model = "claude-sonnet-5";
        // A tenth of the window an hour each, booked at T, out of the band.
        let y = 0.1 / 3_600.0;
        let booking = |entrypoint: Option<&str>| Reservation {
            account_id: "b".into(),
            attempt: 1,
            pin: None,
            model: model.into(),
            entrypoint: entrypoint.map(String::from),
            booked_at: T,
            rewrite: BTreeMap::new(),
            burn: BTreeMap::from([(FIVE_HOUR.to_string(), y)]),
            expires_at: T + HOUR,
        };
        let request = SelectRequest {
            request_id: "r-1".into(),
            attempt: 1,
            seq: 1,
            key_id: "k".into(),
            key_generation: 0,
            agent_id: "main".into(),
            session_id: "s1".into(),
            canonical_model: model.into(),
            folded_model: model.into(),
            candidates: Vec::new(),
            portable: true,
            credit_account: None,
            attempted: Vec::new(),
            now: T,
            received_at: T,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            boundaries: Arc::from([]),
            client: Default::default(),
            entrypoint: None,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            holds: Vec::new(),
            failed_over: None,
            pin_ended: false,
            dialed: false,
            held_out: false,
            lane: lane_key(model, "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
        };
        let read = fit_of(
            &state,
            &snapshot,
            snapshot.pool("p").expect("the case's pool"),
            &request,
            Figures::Fresh(&[]),
            "b",
            &[booking(Some("desk")), booking(None)],
            T,
            Judge::Placement,
            None,
        );
        let projection = read.projection[FIVE_HOUR];
        let expected = 0.3 + y * 1_800.0 + 2.0 * y * 1_800.0 + y * 3_600.0;
        assert!(
            (projection - expected).abs() < 1e-9,
            "the base booking weighs the band's half hour and the desk booking reads one rate: {projection} against \
             {expected}"
        );
    }

    /// The factor of a read at a horizon the end clamped to `now` shows only through an entry write, and no selection
    /// writes one there: the pin path reads an account whose saved end passed as `subscription_ended` and runs no rule,
    /// and the heal check ahead of it reads the stretch and not the factor. The case calls the read itself.
    #[test]
    fn a_horizon_the_end_reached_reads_a_factor_of_one_under_the_ceiling_and_zero_at_it() {
        const HOUR: i64 = 3_600_000;
        const T: i64 = 10 * HOUR;
        let tier = "default_claude_max_20x";
        let mut pool = PoolRow::for_tests("p");
        pool.slow_down.slow_down = true;
        pool.five_hour_ceiling = 0.9;
        let snapshot = RegistrySnapshot::from_load(
            RegistryLoad {
                accounts: vec![AccountRow::for_tests("b", AccountKind::Oauth)],
                pools: vec![pool],
                terms: vec![SubscriptionTerm {
                    account_id: "b".into(),
                    since: None,
                    trial_ends_at: None,
                    cancellation: Cancellation::On {
                        ends_at: Some(T - HOUR),
                    },
                    tier: Some(tier.into()),
                    usage_based: false,
                    plan: Some("max_20x".into()),
                    plan_ended_at: None,
                }],
                ..RegistryLoad::default()
            },
            [0; 32],
            0,
            1,
        );
        let pool = snapshot.pool("p").expect("the case's pool");
        // A booking of a tenth of the window an hour, so the demand alone would read a stretch, and the reset an hour
        // out, two hours past the end, which passed an hour before `now`.
        let read_at = |utilization: f64| {
            let mut state = RoutingState::from_load(RoutingLoad::default());
            state.book_reservation(
                "r-1",
                Reservation {
                    account_id: "b".into(),
                    attempt: 1,
                    pin: None,
                    model: "claude-sonnet-5".into(),
                    entrypoint: None,
                    booked_at: T,
                    rewrite: BTreeMap::new(),
                    burn: BTreeMap::from([(FIVE_HOUR.to_string(), 0.1 / 3_600.0)]),
                    expires_at: T + HOUR,
                },
            );
            state.record_sample(
                "b",
                UtilizationSample {
                    observed_at: T,
                    ..Default::default()
                }
                .with(
                    FIVE_HOUR,
                    WindowReading {
                        utilization,
                        reset: Some(T + HOUR),
                        observed_at: T,
                        source: WindowSource::Head,
                        plan: None,
                        tier: None,
                    },
                ),
                T,
            );
            slow_down_of(&state, &snapshot, pool, "b", tier, T).expect("a read")
        };
        let under = read_at(0.5);
        assert!(under.demand > 0.0, "{under:?}");
        assert_eq!(
            (under.horizon, under.stretch_ms, under.factor),
            (T, 0, 1.0),
            "under the ceiling a reached horizon reads no stretch and a factor of 1"
        );
        let over = read_at(0.97);
        assert_eq!(
            (over.horizon, over.factor),
            (T, 0.0),
            "at the ceiling a reached horizon reads a factor of 0"
        );
        // The booking alone demands 0.1 / 3 600 of the window a second, so the 0.07 over the ceiling takes 2 520 s.
        assert_eq!(
            over.stretch_ms, 2_520_000,
            "the stretch is the overrun at the demand, with no time left to the horizon"
        );
    }
}

#[cfg(test)]
mod runway_tests {
    use std::collections::BTreeMap;

    use super::{runway_ties, Candidate, FitRead, FitWord};

    fn candidate(account_id: &str) -> Candidate {
        Candidate {
            account_id: account_id.into(),
            incarnation: 1,
            priority: 0,
            upstream_model: String::new(),
        }
    }

    fn fits(runway_ms: i64) -> FitRead {
        FitRead {
            verdict: FitWord::Fits,
            runway_ms: Some(runway_ms),
            binding: None,
            rank_runway_ms: Some(runway_ms),
            wall_runway_ms: None,
            wall_shown_ms: None,
            wall_binding: None,
            projection: BTreeMap::new(),
            horizon: BTreeMap::new(),
            rewrite: BTreeMap::new(),
            reserved: BTreeMap::new(),
            end: None,
            words: BTreeMap::new(),
        }
    }

    /// The selection and the shedding round hand the tie break a read for each tie or for none, so the case calls it
    /// with a mix.
    #[test]
    fn a_tie_with_no_read_stays_beside_the_longest_runway_and_a_shorter_one_leaves() {
        let (short, long, unread) = (candidate("a"), candidate("b"), candidate("c"));
        let reads = vec![
            ("a".to_string(), fits(60_000)),
            ("b".to_string(), fits(120_000)),
        ];
        let kept = runway_ties(&[&short, &long, &unread], &reads);
        assert_eq!(
            kept.iter()
                .map(|c| c.account_id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"],
            "nothing says the unread tie's runway is shorter"
        );
    }
}

#[cfg(test)]
mod plan_window_tests {
    use super::{all_plan_windows, fit_key};

    /// A fact's key is static, so `fit_key` spells the keys per window by hand while the windows come from
    /// `MODEL_WINDOWS`: a row added to the table fails here until the keys are spelled, rather than leaving its week
    /// unnamed in the record.
    #[test]
    fn every_projected_window_has_its_fact_keys() {
        let projected: Vec<&str> = all_plan_windows().collect();
        for window in &projected {
            for kind in ["burn", "horizon", "projection", "rewrite", "reserved"] {
                assert!(
                    fit_key(kind, window).is_some(),
                    "{kind} has no fact key on {window}"
                );
            }
        }
    }
}
