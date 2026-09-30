//! One selection: the spec's seven steps over the snapshot and the routing state.

use phoebis_store::{
    AccountKind, AccountRow, AccountState, AffinityMode, OveragePolicy, PinBurn, PinKey, PinRow,
    PoolRow, SessionPlacement, Strategy, SubagentPlacement, WarmSessions,
};
use rand::{Rng, RngExt};

use crate::affinity::{AffinityEntry, AffinityMap};
use crate::state::{
    budget_deadline, shed_marks, warm, BurnUnit, CreditsState, PinRef, RoutingState, StateWrite,
    Tier, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE,
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
    pub key_id: String,
    /// The authenticated key's `created_at` at admission. The id is reusable, so the lifetime is the identity: a
    /// stale-revision re-authentication that finds another generation under the id refuses the request.
    pub key_generation: i64,
    /// The identity's agent: `main`, or a subagent's own id, so a pin is held per agent of a session.
    pub agent_id: String,
    pub session_id: Option<String>,
    pub canonical_model: String,
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
    pub boundaries: Vec<String>,
    pub client: ClientClass,
    /// Every static boundary in chain order; `static_hash` stays the last one.
    pub static_boundaries: Vec<String>,
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
}

/// The holds before this attempt, as the trace records them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CohortHold {
    pub waited_on: Vec<String>,
    pub cohort: Option<String>,
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

/// The verdict of `request` on the pin it arrives for. The compacted flag is cold whatever else the request says; a
/// request on another cache lane than the pin's last is cold whatever its messages continue, unless it leaves the
/// pin alone (a class that does not refresh, or a foreign request); the class decides when present; without it, a
/// request whose boundaries contain the pin's tail or name an entry the pin's key and generation wrote on the pin's
/// account and incarnation continues the conversation (so where one session holds several conversations, a
/// continuation of any of them refreshes the pin), one that matches the static prefix without such a boundary
/// restarted it, and one matching
/// neither is foreign. A body without a chain refreshes, the fallback for a request that carries no fact. A
/// keepalive probe replays the recipe's headers and body verbatim, the re-arming request's rather than its own, so
/// it is judged by none of this: its renewal report stamps the refresh instead. A token count is judged by none of
/// it either, since it creates no cache entry to be warm about.
pub fn warmth_of(current: &PinRow, request: &SelectRequest, affinity: &AffinityMap) -> Warmth {
    if request.probe || request.count {
        return Warmth::Untouched;
    }
    if request.context_compacted {
        return Warmth::Cold;
    }
    // Another lane is another cache: a refreshing turn on it reads the pin cold, whatever its boundaries. A request
    // that leaves the pin alone still does, since it never touched the conversation's prefix.
    let lane_changed = !same_lane(&current.lane, &request.lane);
    let refreshed = if lane_changed {
        Warmth::Cold
    } else {
        Warmth::Refresh
    };
    if let Some(class) = request.request_class.as_deref() {
        return if REFRESHING_CLASSES.contains(&class) {
            refreshed
        } else {
            Warmth::Untouched
        };
    }
    if request.static_hash.is_none() && request.boundaries.is_empty() {
        return refreshed;
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
        return refreshed;
    }
    if request.static_hash.is_some() && request.static_hash == current.static_hash {
        return Warmth::Cold;
    }
    Warmth::Untouched
}

/// The pin's warmth on this request: `Refresh` is warm, `Cold` is cold, `Untouched` reads `warm(current, now)`.
pub fn effective_warm(current: &PinRow, request: &SelectRequest, affinity: &AffinityMap) -> bool {
    warm_on(current, warmth_of(current, request, affinity), request.now)
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

/// The hysteresis band under the migrate threshold: a destination counts only below `threshold - MIGRATE_BAND`.
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
    /// A step-2 obstacle (`attempted`, `budget`, `cooldown`, `credits`, `operator`, `overage`,
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
}

/// The reason a rejection cooldown row carries; the verdict writes it and the report below reads it.
pub const MODEL_REJECTED_REASON: &str = "model_rejected";

/// The reason a fast request's 429 that names an overage-disabled reason cools the fast lane under; the row's
/// `window` is that reason, which the local answer hands back to the client.
pub const FAST_CREDITS_REASON: &str = "fast_credits";

/// The reason a fast request's 429 without that reason cools the fast lane under; the row's `window` is `fast`, or
/// `retry_after` when only `Retry-After` timed it, so the in-place retry treats it as any timed throttle.
pub const FAST_RATE_LIMIT_REASON: &str = "fast_rate_limit";

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
    /// The member's spend reached its configured budget; `until` is the period's end.
    Budget {
        until: i64,
    },
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

/// The step-2 name `release_reasons` and the trace report for an obstacle: `attempted`, `budget`, `cooldown`,
/// `credits`, `operator`, `overage`, or `subscription_ended`.
fn obstacle_name(obstacle: &Obstacle) -> &'static str {
    match obstacle {
        Obstacle::Attempted => "attempted",
        Obstacle::Budget { .. } => "budget",
        Obstacle::Cooldown { reason, .. } if reason == "overage" => "overage",
        Obstacle::Cooldown { .. } => "cooldown",
        Obstacle::Credits { .. } => "credits",
        Obstacle::Ended => "subscription_ended",
        Obstacle::Operator => "operator",
    }
}

/// The member's spend at or above its configured budget, still inside the bounds the ledger reported it for:
/// answers the period's end. A row from another period than the member's configured one, or whose bounds do not
/// hold `now`, is no obstacle, and neither is a member with no budget configured.
fn budget_exhausted(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool_id: &str,
    account_id: &str,
    now: i64,
) -> Option<i64> {
    let member = snapshot
        .members(pool_id)
        .iter()
        .find(|m| m.account_id == account_id)?;
    let spend = state.member_spend(pool_id, account_id)?;
    budget_deadline(member, spend, now)
}

/// Whether an account can still serve a request whose allowance is judged in `window`: the model's own weekly
/// window for a fable request, both shared plan windows otherwise, either of whose two predictions at 1.0 is
/// spent. No sample, or a sample that never read the window, counts as allowance (an api_key account, whose
/// sample carries no plan window of its own, always holds it here); a reading past its reset, or one a newer
/// head lowered, no longer excludes.
fn holds_allowance(state: &RoutingState, account_id: &str, window: &str, now: i64) -> bool {
    let spent = |name: &str| {
        state
            .predicted(account_id, name, now)
            .is_some_and(|r| r.utilization >= 1.0)
    };
    if window == SEVEN_DAY_FABLE {
        !spent(SEVEN_DAY_FABLE)
    } else {
        !spent(FIVE_HOUR) && !spent(SEVEN_DAY)
    }
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
/// account), `fast_rate_limit` or `fast_credits` for a fast request (the fast lane's own row, or the poll's
/// credits state, blocks it), `budget` (the member's spend is at or above its configured budget), `overage` (in
/// overage under a policy other than `allow`), `attempted` (this request already tried it), and `spent` (no
/// allowance left in the request's window); a fast request draws no plan window, so neither `overage` nor
/// `spent` stands against it. Never empty: a pin released by a move whose destination then answered nothing
/// carries `released`, the one fact the list above does not evaluate.
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
        reasons.push(match (row.scope.as_str(), row.reason.as_str()) {
            ("fast", FAST_CREDITS_REASON) => FAST_CREDITS_REASON,
            ("fast", _) => FAST_RATE_LIMIT_REASON,
            _ => "cooldown",
        });
    }
    if request.fast
        && state
            .credits(&pin.account_id)
            .and_then(CreditsState::blocked)
            .is_some()
        && !reasons.contains(&FAST_CREDITS_REASON)
    {
        reasons.push(FAST_CREDITS_REASON);
    }
    if pool.is_some_and(|p| {
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

pub fn select(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    request: &SelectRequest,
    rng: &mut impl Rng,
) -> (SelectResult, Vec<StateWrite>) {
    let mut writes = Vec::new();
    let Some(pool) = snapshot
        .key(&request.key_id)
        .and_then(|k| snapshot.pool(&k.pool_id))
    else {
        return (SelectResult::Exhausted(Exhaustion::Untimed), writes);
    };
    let ttl_ms = affinity_ttl_ms(pool);
    // A marker-less request writes no cache, so under `when_cached` it has no pin to keep and spreads by the
    // strategy instead of pinning; `always`, a marked request, or a non-portable one still pins.
    let session = request.session_id.as_deref().filter(|_| {
        ttl_ms > 0
            && (pool.affinity.mode == AffinityMode::Always || request.marker || !request.portable)
    });

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
            if idle || (!serves && request.portable) {
                state.remove_pin(&key);
                writes.push(StateWrite::DeletePin(key));
            } else {
                pin = Some(current);
            }
        }
    }

    // The fast answer names the pinned account's credits reason first, whatever becomes of the pin below.
    let pinned_account = pin.as_ref().map(|p| p.account_id.clone());

    // What the request says about the pin's cache, judged once where the pin is: every rule below that reads
    // warmth reads this, so a compacted or restarted request is cold on the request that carries the fact.
    let verdict = pin.as_ref().map_or(Warmth::Untouched, |current| {
        warmth_of(
            current,
            request,
            &state.affinity().read().expect("affinity lock poisoned"),
        )
    });

    // Step 2: the eligible set and the obstacle of everything outside it, each kept with its account id for
    // the release event and the trace.
    let mut eligible: Vec<&Candidate> = Vec::new();
    let mut obstacles: Vec<(String, Obstacle)> = Vec::new();
    for candidate in &request.candidates {
        match obstacle(
            state,
            snapshot,
            pool,
            candidate,
            &request.canonical_model,
            &request.attempted,
            request.now,
            request.fallback_ms,
            request.fast,
        ) {
            None => eligible.push(candidate),
            Some(o) => obstacles.push((candidate.account_id.clone(), o)),
        }
    }
    let in_set = |set: &[&Candidate], id: &str| set.iter().any(|c| c.account_id == id);

    // A probe warms the pin it was sent for. With that member at its spend budget there is nothing to warm, and a
    // placement on a sibling would move the session's pin onto a member no request asked for, so the probe
    // answers the budget's deadline and moves nothing. The window names the obstacle for the relay, which
    // renders it on the wire as the overage claim and hands the keepalive the name.
    if request.probe {
        if let Some(current) = pin.as_ref() {
            if let Some((_, Obstacle::Budget { until })) = obstacles
                .iter()
                .find(|(id, o)| id == &current.account_id && matches!(o, Obstacle::Budget { .. }))
            {
                return (
                    SelectResult::Exhausted(Exhaustion::Timed {
                        deadline: *until,
                        window: "budget".into(),
                    }),
                    writes,
                );
            }
        }
    }

    // Step 3: a non-portable request is bound to its pin.
    if let (false, Some(current)) = (request.portable, pin.as_ref()) {
        if in_set(&eligible, &current.account_id) {
            return (
                SelectResult::Selected(renew(
                    state,
                    snapshot,
                    pool,
                    current,
                    request,
                    verdict,
                    &mut writes,
                )),
                writes,
            );
        }
        let exhaustion = fenced(
            state,
            snapshot,
            pool,
            request,
            &current.account_id,
            current.incarnation,
        );
        return (SelectResult::Exhausted(exhaustion), writes);
    }

    let window = window_for(&request.canonical_model);

    // Step 4: the moves, for a portable request with a live pin. A pin on an account past its saved end never
    // reaches them: step 5 releases it as it releases a cooled account's.
    let mut migrated = None;
    let mut shed_from = None;
    let mut selection_set: Vec<&Candidate> = eligible.clone();
    if let Some(current) = pin.as_ref().filter(|current| {
        !snapshot
            .term(&current.account_id)
            .is_some_and(|term| term::ended(term, request.now))
    }) {
        let cold = !warm_on(current, verdict, request.now);
        // The lane the pin carries once this request renews it: a request that leaves the pin alone stamps none.
        let fast = if verdict == Warmth::Untouched {
            lane_is_fast(&current.lane)
        } else {
            request.fast
        };
        // A hold is for a fresh placement: with every destination at its cap the move is not made, the pin
        // renews in place at step 5, and the next request judges the move again.
        if let Some((reason, set)) = move_of(
            state, snapshot, pool, request, current, &eligible, window, cold, fast,
        )
        .filter(|(_, set)| {
            set.iter()
                .any(|c| state.in_flight().account(&c.account_id) < pool.failover.in_flight_cap)
        }) {
            state.remove_pin(&current.key());
            writes.push(StateWrite::DeletePin(current.key()));
            if reason == MigrateReason::Shed {
                shed_from = Some(current.account_id.clone());
            }
            migrated = Some(reason);
            selection_set = set;
            pin = None;
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
                    verdict,
                    &mut writes,
                )),
                writes,
            );
        }
    }

    // Step 6: a model with its own window needs allowance left in it, the cap bounds a fresh placement, the
    // ceiling prefers the accounts under it, the wind-down prefers accounts that can take a fresh session, and
    // under Pace the floor keeps an almost-spent account as the last resort. Each preference falls through to
    // the wider set it narrowed when it would otherwise empty the selection, and every candidate a partition
    // drops joins the trace under that partition's name.
    let mut skipped: Vec<Skipped> = Vec::new();
    // A dated deadline for the accounts the Fable partition drops, kept apart from `skipped`: it answers
    // `exhaustion_of` when nothing is left to choose from, but every dropped candidate joins the trace as
    // `spent` whether or not its reading names a reset, so the deadline never doubles as its own trace label.
    let mut spent_deadlines: Vec<(String, Obstacle)> = Vec::new();
    // The candidates placement may choose among, before any of step 6's own partitions: judged on the account
    // itself, never on whether a preference below narrowed the wider set around it.
    let allowed: Vec<&Candidate> = selection_set.clone();
    if window == SEVEN_DAY_FABLE {
        let (left, spent): (Vec<&Candidate>, Vec<&Candidate>) = selection_set
            .iter()
            .copied()
            .partition(|c| holds_allowance(state, &c.account_id, window, request.now));
        for c in &spent {
            skipped.push(Skipped {
                account_id: c.account_id.clone(),
                obstacle: "spent",
            });
            if let Some(until) = state
                .predicted(&c.account_id, SEVEN_DAY_FABLE, request.now)
                .and_then(|r| r.reset)
            {
                spent_deadlines.push((
                    c.account_id.clone(),
                    Obstacle::Cooldown {
                        reason: "spent".into(),
                        until,
                        window: SEVEN_DAY_FABLE.to_string(),
                    },
                ));
            }
        }
        selection_set = left;
    }
    // An account at its cap takes no fresh placement: a bound rather than a preference, so it runs ahead of the
    // preferences below and a free account the ceiling would pass over still takes the request. With every
    // account that could serve at its cap the request is held for a slot; the set is left whole so the answer
    // names the account the strategy would have chosen. A pinned request never gets here: step 5 renewed it.
    let under_cap: Vec<&Candidate> = selection_set
        .iter()
        .copied()
        .filter(|c| state.in_flight().account(&c.account_id) < pool.failover.in_flight_cap)
        .collect();
    let held = under_cap.is_empty();
    narrow(&mut selection_set, &mut skipped, under_cap, "at_cap");
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
        narrow(&mut selection_set, &mut skipped, below, "above_ceiling");
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
    narrow(
        &mut selection_set,
        &mut skipped,
        not_winding_down,
        "winding_down",
    );
    // An almost-spent account is kept for when nothing else can serve.
    if pool.strategy == Strategy::Pace {
        let above_floor: Vec<&Candidate> = selection_set
            .iter()
            .copied()
            .filter(|c| {
                remaining(state, &c.account_id, window, request.now).0 > pool.failover.pace_floor
            })
            .collect();
        narrow(&mut selection_set, &mut skipped, above_floor, "floor");
    }
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
            request.now,
            request.fallback_ms,
            !held,
            rng,
        ) else {
            let (pinned, others): (Vec<_>, Vec<_>) = obstacles
                .into_iter()
                .chain(spent_deadlines)
                .partition(|(id, _)| Some(id) == pinned_account.as_ref());
            let obstacles = pinned.into_iter().chain(others).map(|(_, o)| o).collect();
            return (SelectResult::Exhausted(exhaustion_of(obstacles)), writes);
        };
        if held {
            // The cohort-aware hold is a cap exemption and nothing more: a session already on an account under
            // any agent lands there rather than waiting, and a still-held subagent waits where its own cohort
            // is rather than on the strategy's unrelated choice. A token count takes neither: it is never
            // placed, so its session's pins and cohort have no say in where it lands or waits.
            let session_pin = (!request.count)
                .then(|| any_agent_pin(state, request, &allowed, window, ttl_ms))
                .flatten();
            if let Some(candidate) = session_pin {
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
                let cohort = (!request.count)
                    .then(|| {
                        subagent_cohort(
                            state,
                            snapshot,
                            pool,
                            request,
                            &affinity_guard,
                            &allowed,
                            window,
                        )
                    })
                    .flatten();
                let hold = match cohort {
                    Some(c) => HoldFacts {
                        account_id: c.account_id.clone(),
                        cohort: Some(c.account_id.clone()),
                    },
                    None => HoldFacts {
                        account_id: strategy_choice.account_id.clone(),
                        cohort: None,
                    },
                };
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
    let cohort_hold = (!request.holds.is_empty()).then(|| CohortHold {
        waited_on: request.holds.iter().map(|h| h.account_id.clone()).collect(),
        cohort: request.holds.last().and_then(|h| h.cohort.clone()),
    });

    // Step 7: a fresh pin with a fresh epoch.
    let pin_ref = session.map(|session| {
        let epoch = state.fresh_epoch();
        let row = fresh_pin(request, session, epoch, chosen, shed_from.clone());
        state.put_pin(row.clone());
        writes.push(StateWrite::UpsertPin(row));
        PinRef {
            key_id: request.key_id.clone(),
            session_id: session.to_string(),
            agent_id: request.agent_id.clone(),
            model: request.canonical_model.clone(),
            epoch,
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
    exhaustion_of(own.into_iter().collect())
}

fn seconds_ms(seconds: u64) -> i64 {
    i64::try_from(seconds)
        .ok()
        .and_then(|s| s.checked_mul(1_000))
        .unwrap_or(i64::MAX)
}

fn reading_at_least(
    state: &RoutingState,
    account_id: &str,
    name: &str,
    threshold: f64,
    now: i64,
) -> bool {
    state
        .predicted(account_id, name, now)
        .is_some_and(|r| r.utilization >= threshold)
}

/// An unsampled account, or one that never read the window, counts as under.
fn reading_under(
    state: &RoutingState,
    account_id: &str,
    name: &str,
    threshold: f64,
    now: i64,
) -> bool {
    state
        .predicted(account_id, name, now)
        .is_none_or(|r| r.utilization < threshold)
}

/// Whether `account_id` reads under `migrate_above_utilization` minus the hysteresis band: the shared plan
/// windows' predictions for a subscription account, the worst standing bucket for an api_key one. No threshold
/// configured, or no reading, counts as under. The same reading `move_of`'s `cool` predicate takes for a migration
/// destination, so the arithmetic lives once.
fn under_band(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    account_id: &str,
    now: i64,
) -> bool {
    let Some(threshold) = pool.migrate_above_utilization else {
        return true;
    };
    let band = threshold - MIGRATE_BAND;
    match snapshot.account(account_id).map(|a| a.kind) {
        Some(AccountKind::ApiKey) => state
            .sample(account_id)
            .and_then(|s| s.worst_bucket(now))
            .is_none_or(|r| r.utilization < band),
        _ => {
            reading_under(state, account_id, FIVE_HOUR, band, now)
                && reading_under(state, account_id, SEVEN_DAY, band, now)
        }
    }
}

/// Whether the account's allowance is known: an api_key account has no plan window to read, and a subscription
/// account holds a prediction of the request's window, `SEVEN_DAY_FABLE` for a Fable request and both shared
/// windows otherwise.
fn allowance_known(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    account_id: &str,
    window: &str,
    now: i64,
) -> bool {
    let known = |name: &str| state.predicted(account_id, name, now).is_some();
    snapshot.account(account_id).map(|a| a.kind) == Some(AccountKind::ApiKey)
        || if window == SEVEN_DAY_FABLE {
            known(SEVEN_DAY_FABLE)
        } else {
            known(FIVE_HOUR) && known(SEVEN_DAY)
        }
}

/// Step 4's rules in their order: spent, overage, return to plan, drain, shed. Each yields the siblings it
/// would move the pin to, and the first with any moves it under its own reason.
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
) -> Option<(MigrateReason, Vec<&'a Candidate>)> {
    let (now, fallback_ms) = (request.now, request.fallback_ms);
    let prefer_plan = pool.overage_policy == OveragePolicy::PreferPlan;
    let kind = |id: &str| snapshot.account(id).map(|a| a.kind);
    let dwell_ms = seconds_ms(pool.shedding.source_dwell_seconds);
    // A sibling the next round would not shed from again soon: every reading its sample holds under the band
    // (the shared windows of a subscription account, the worst live bucket of an api_key one; an unsampled
    // account reads as under), no round of its own within the dwell, and room under the cap. A full sibling
    // counts as hot, so a move whose cool siblings are all full falls through to a free one rather than being
    // dropped at the cap.
    let cool = |state: &RoutingState, c: &Candidate| -> bool {
        under_band(state, snapshot, pool, &c.account_id, now)
            && state
                .last_shed(&c.account_id)
                .is_none_or(|at| now - at >= dwell_ms)
            && state.in_flight().account(&c.account_id) < pool.failover.in_flight_cap
    };
    let destinations =
        |state: &RoutingState, keep: &dyn Fn(&Candidate) -> bool| -> Vec<&'a Candidate> {
            let set: Vec<&'a Candidate> = eligible
                .iter()
                .copied()
                .filter(|c| c.account_id != current.account_id && keep(c))
                // An account with no allowance left in the model's own window is no destination: step 6 drops it,
                // so releasing the pin for a set of them would strand the request on nothing.
                .filter(|c| {
                    window != SEVEN_DAY_FABLE || holds_allowance(state, &c.account_id, window, now)
                })
                .collect();
            // A hot destination would shed the session again at its next round, so every move prefers a cool
            // one. The preference falls through to the rule's own set: each rule moves the pin for a reason a
            // hot sibling still answers, which staying put does not.
            if pool.migrate_above_utilization.is_none() {
                return set;
            }
            let preferred: Vec<&'a Candidate> =
                set.iter().copied().filter(|c| cool(state, c)).collect();
            if preferred.is_empty() {
                set
            } else {
                preferred
            }
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
    if !request.fast
        && pool.shedding.migrate_when_spent
        && !holds_allowance(state, &current.account_id, window, now)
    {
        let set = destinations(state, &|c| {
            holds_allowance(state, &c.account_id, window, now)
                && allowance_known(state, snapshot, &c.account_id, window, now)
                && !(prefer_plan && state.in_overage(&c.account_id, now, fallback_ms))
        });
        if !set.is_empty() {
            return Some((MigrateReason::Spent, set));
        }
    }

    // A subscription account in overage hands the session back to plan quota, warm or cold, onto a plan-funded
    // subscription sibling: one out of overage with allowance left, since a spent one would move the pin again
    // on its next request. While the evidence's reset falls within the grace the session keeps its cache: its
    // account is about to get its plan back, and moving now would move it twice.
    if !request.fast
        && prefer_plan
        && kind(&current.account_id) == Some(AccountKind::Oauth)
        && state.in_overage(&current.account_id, now, fallback_ms)
    {
        let grace_ms = i64::from(pool.failover.overage_move_grace_minutes) * 60_000;
        let reset = state.overage(&current.account_id).and_then(|e| e.reset);
        if reset.is_none_or(|reset| reset - now > grace_ms) {
            let set = destinations(state, &|c| {
                kind(&c.account_id) == Some(AccountKind::Oauth)
                    && !state.in_overage(&c.account_id, now, fallback_ms)
                    && holds_allowance(state, &c.account_id, window, now)
            });
            if !set.is_empty() {
                return Some((MigrateReason::Overage, set));
            }
        }
    }

    // An api_key member bills every token, so a pin on one returns to plan quota once its cache is cold or once
    // it has sat a dwell on the member. A move writes a fresh row, so `created_at` is the landing here. The
    // member names no reset, so no grace applies.
    if kind(&current.account_id) == Some(AccountKind::ApiKey)
        && (cold || now - current.created_at >= dwell_ms)
    {
        let set = destinations(state, &|c| {
            kind(&c.account_id) == Some(AccountKind::Oauth)
                && !state.in_overage(&c.account_id, now, fallback_ms)
                && holds_allowance(state, &c.account_id, window, now)
        });
        if !set.is_empty() {
            return Some((MigrateReason::ReturnToPlan, set));
        }
    }

    // A cold pin costs nothing to move off an account about to end. A warm one stays: past the end the
    // `subscription_ended` exclusion releases it and the upstream refuses the dead token.
    let winding_down = |id: &str| {
        snapshot
            .term(id)
            .is_some_and(|term| term::winding_down(term, now, pool.failover.wind_down_minutes))
    };
    if cold && winding_down(&current.account_id) {
        let set = destinations(state, &|c| !winding_down(&c.account_id));
        if !set.is_empty() {
            return Some((MigrateReason::Drain, set));
        }
    }

    // The trigger reads the prediction, as the ceiling does: a refunded window reads zero at its reset.
    let threshold = pool.migrate_above_utilization?;
    let hot = reading_at_least(state, &current.account_id, FIVE_HOUR, threshold, now)
        || reading_at_least(state, &current.account_id, SEVEN_DAY, threshold, now);
    if !hot {
        return None;
    }
    if state
        .last_shed(&current.account_id)
        .is_none_or(|at| now - at >= dwell_ms)
    {
        let marks = shed_round(state, snapshot, pool, current, cold, fast, now, fallback_ms);
        state.record_shed(&current.account_id, now, marks);
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
        return None;
    }
    // A pin moves only when its session asks, so an unmarked pin never moves for utilization and a marked one
    // moves at its next request. A plan-quota round sheds window share, which a fast request does not draw, so
    // its mark moves a standard request alone; a dollar round's mark moves a fast one too.
    if !state.shed_marked(&current.key(), now, dwell_ms)
        || (request.fast
            && burn_unit(state, snapshot, &current.account_id, now, fallback_ms)
                == BurnUnit::WindowShare)
    {
        return None;
    }
    let set = destinations(state, &|c| {
        let shed_it_here = current.shed_from.as_deref() == Some(c.account_id.as_str())
            && current.shed_at.is_some_and(|at| now - at < dwell_ms);
        cool(state, c) && !shed_it_here
    });
    (!set.is_empty()).then_some((MigrateReason::Shed, set))
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

/// A shedding round over the account `current` sits on: every pin of it, cold first by burn, then warm by burn
/// over rewrite cost, bounded by `warm_sessions`, marked in that order up to `shed_share` of the account's burn
/// in the unit its funding selects. On plan quota each burn is the Max 5x 5-hour share rate: the tier's factor
/// divides every pin of the account alike, so neither the order nor the share of the sum needs it, and a pin on
/// a fast lane is out of the round, since it draws no window and has nothing to shed there; in dollars it stays.
/// The current pin's warmth and lane are this request's; every other pin's are the stored ones.
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
) -> Vec<PinKey> {
    let account_id = &current.account_id;
    let unit = burn_unit(state, snapshot, account_id, now, fallback_ms);
    let (mut pins, mut sum) = state.burn_of(
        account_id,
        unit,
        state.rates(),
        now,
        seconds_ms(pool.shedding.burn_window_seconds),
    );
    let is_current = |p: &PinRow| p.key() == current.key();
    if unit == BurnUnit::WindowShare {
        let on_fast_lane = |p: &PinRow| {
            if is_current(p) {
                current_fast
            } else {
                lane_is_fast(&p.lane)
            }
        };
        pins.retain(|(p, _)| !on_fast_lane(p));
        sum = pins.iter().map(|(_, burn)| burn).sum();
    }
    let (mut cold, mut warm_pins): (Vec<_>, Vec<_>) = pins.into_iter().partition(|(p, _)| {
        if is_current(p) {
            current_cold
        } else {
            !warm(p, now)
        }
    });
    // A pin with no prompt size reported yet ranks as the cheapest rewrite.
    let per_rewrite = |(p, burn): &(PinRow, f64)| *burn / p.last_input_tokens.max(1) as f64;
    let by_rewrite =
        |a: &(PinRow, f64), b: &(PinRow, f64)| per_rewrite(b).total_cmp(&per_rewrite(a));
    cold.sort_by(|a, b| b.1.total_cmp(&a.1));
    warm_pins.sort_by(by_rewrite);
    let cold_count = cold.len();
    let mut ordered = cold;
    ordered.extend(warm_pins);
    if pool.shedding.warm_sessions == WarmSessions::Always {
        ordered.sort_by(by_rewrite);
    }
    let mut marks = shed_marks(&ordered, pool.shedding.shed_share * sum);
    // `never` stops after the cold pins. The warm ones stay in the order all the same, so the last-pin rule
    // still counts them among the account's pins.
    if pool.shedding.warm_sessions == WarmSessions::Never {
        marks.truncate(cold_count);
    }
    marks
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
    if !account.enabled || account.state != AccountState::Ready {
        return Some(Obstacle::Operator);
    }
    if snapshot
        .term(&candidate.account_id)
        .is_some_and(|term| term::ended(term, now))
    {
        return Some(Obstacle::Ended);
    }
    if let Some(cooldown) = state.blocking_cooldown(&candidate.account_id, model, now, fast) {
        return Some(Obstacle::Cooldown {
            reason: cooldown.reason.clone(),
            until: cooldown.until,
            window: cooldown.window.clone(),
        });
    }
    if fast {
        if let Some(reason) = state
            .credits(&candidate.account_id)
            .and_then(CreditsState::blocked)
        {
            return Some(Obstacle::Credits {
                reason: reason.into(),
            });
        }
    }
    if let Some(until) = budget_exhausted(state, snapshot, &pool.id, &candidate.account_id, now) {
        return Some(Obstacle::Budget { until });
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
fn renew(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    current: &PinRow,
    request: &SelectRequest,
    verdict: Warmth,
    writes: &mut Vec<StateWrite>,
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
        }),
        migrated: None,
        in_overage: state.in_overage(&current.account_id, now, fallback_ms),
        trace: SelectionTrace {
            strategy: pool.strategy.as_str(),
            renewed: true,
            ..SelectionTrace::default()
        },
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
/// stranding the request when it would empty the set), recording every dropped candidate under `label`.
fn narrow<'a>(
    selection_set: &mut Vec<&'a Candidate>,
    skipped: &mut Vec<Skipped>,
    kept: Vec<&'a Candidate>,
    label: &'static str,
) {
    if kept.is_empty() {
        return;
    }
    for c in selection_set.iter() {
        if !kept.iter().any(|k| k.account_id == c.account_id) {
            skipped.push(Skipped {
                account_id: c.account_id.clone(),
                obstacle: label,
            });
        }
    }
    *selection_set = kept;
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

/// An affinity entry placement may land on: live, under the snapshot's incarnation for its account, and the
/// account itself admits a fresh placement. Shared by every rung; rung 4 adds its own blocker on top.
fn account_ok(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    allowed: &[&Candidate],
    window: &str,
    now: i64,
    entry: &AffinityEntry,
) -> bool {
    entry.live(now)
        && snapshot.account(&entry.account_id).map(|a| a.incarnation) == Some(entry.incarnation)
        && admits(state, allowed, window, &entry.account_id, now)
}

/// Rung 4's own gate on a candidate account, evaluated per candidate rather than inferred from the
/// post-partition selection set: under the cap, below `select_below_utilization` on the 5-hour window when the
/// pool sets one, not winding down, above the Pace floor under Pace, and under the band `move_of`'s `cool` reads
/// for a migration destination.
fn blocked(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    now: i64,
    account_id: &str,
) -> bool {
    state.in_flight().account(account_id) >= pool.failover.in_flight_cap
        || pool
            .select_below_utilization
            .is_some_and(|threshold| reading_at_least(state, account_id, FIVE_HOUR, threshold, now))
        || snapshot
            .term(account_id)
            .is_some_and(|term| term::winding_down(term, now, pool.failover.wind_down_minutes))
        || (pool.strategy == Strategy::Pace
            && remaining(state, account_id, window, now).0 <= pool.failover.pace_floor)
        || !under_band(state, snapshot, pool, account_id, now)
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
) -> Option<(&'a Candidate, Placement)> {
    if request.count {
        return None;
    }
    let now = request.now;
    let candidate_of =
        |account_id: &str| allowed.iter().copied().find(|c| c.account_id == account_id);

    if let Some((depth, entry)) =
        affinity.deepest(&request.canonical_model, &request.boundaries, |e| {
            same_lane(&e.lane, &request.lane)
                && account_ok(state, snapshot, allowed, window, now, e)
        })
    {
        if let Some(candidate) = candidate_of(&entry.account_id) {
            return Some((
                candidate,
                Placement {
                    rung: "content",
                    depth,
                    prefix_tokens: entry.prefix_tokens,
                },
            ));
        }
    }

    let is_subagent = request.agent_id != "main";
    if is_subagent && pool.affinity.subagent_placement == SubagentPlacement::WithMain {
        let session_id = request.session_id.as_deref()?;
        let key = PinKey {
            key_id: request.key_id.clone(),
            session_id: session_id.to_string(),
            agent_id: request
                .parent_agent_id
                .clone()
                .unwrap_or_else(|| "main".to_string()),
            model: request.canonical_model.clone(),
        };
        let pin = state.pin(&key)?;
        if now - pin.last_used_at > affinity_ttl_ms(pool)
            || !admits(state, allowed, window, &pin.account_id, now)
        {
            return None;
        }
        let candidate = candidate_of(&pin.account_id)?;
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
    if !by_warm_prefix || (!request.marker && request.portable) {
        return None;
    }
    // An entry whose estimate is zero (the empty-tools boundary of a request without tools) never places for
    // any class; the threshold itself binds a main conversation only, since a subagent cohort lands together on
    // any shared prefix.
    let size_gate = |entry: &AffinityEntry| {
        entry.prefix_tokens > 0
            && (is_subagent || entry.prefix_tokens >= pool.affinity.min_shared_prefix_tokens)
    };
    let (depth, entry) =
        affinity.deepest(&request.canonical_model, &request.static_boundaries, |e| {
            same_lane(&e.lane, &request.lane)
                && account_ok(state, snapshot, allowed, window, now, e)
                && size_gate(e)
                && !blocked(state, snapshot, pool, window, now, &e.account_id)
        })?;
    let candidate = candidate_of(&entry.account_id)?;
    Some((
        candidate,
        Placement {
            rung: "static",
            depth,
            prefix_tokens: entry.prefix_tokens,
        },
    ))
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
    allowed.iter().copied().find(|c| c.account_id == account_id)
}

/// The cohort-aware hold's second override: a subagent still held after the first waits where its own cohort
/// is, so the wait frees a slot there rather than on the strategy's unrelated choice. Gated as rung 4 is (the
/// zero-size entry, the class knob, the cache gate), so an empty-tools boundary or a strategy-only pool never
/// names a false cohort.
fn subagent_cohort<'a>(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    request: &SelectRequest,
    affinity: &AffinityMap,
    allowed: &[&'a Candidate],
    window: &str,
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
    allowed
        .iter()
        .copied()
        .find(|c| c.account_id == entry.account_id)
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

/// `places` is false for a request the cap holds: the choice names the account it waits for and places nothing.
#[allow(clippy::too_many_arguments)]
fn choose<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    set: &[&'a Candidate],
    now: i64,
    fallback_ms: i64,
    places: bool,
    rng: &mut impl Rng,
) -> Option<(&'a Candidate, Tier)> {
    let kind_ordered = pool.kind_ordered();
    // Group one: oauth with plan quota; two: oauth in overage; three: api_key. The split holds under any
    // overage_policy, because it orders by cost, not eligibility: `allow` still demotes an overage oauth
    // account behind a plan-quota one. `overage_policy` governs eligibility everywhere and orders the tier
    // itself only when the kind order is off, in the `false` arm below.
    let group_of = |c: &Candidate, in_overage: bool| -> u8 {
        let kind = snapshot.account(&c.account_id).map(|a| a.kind);
        match (kind_ordered, kind, in_overage) {
            (true, Some(AccountKind::ApiKey), _) => 2,
            (true, _, true) => 1,
            (true, _, false) => 0,
            (false, _, in_overage) => {
                u8::from(pool.overage_policy == OveragePolicy::PreferPlan && in_overage)
            }
        }
    };
    // The kind order puts the group ahead of the priority; within a group, `prefer_plan` still orders an
    // overage account behind a plan-quota one, group three (api_key) included.
    let key_of = |c: &Candidate| -> Tier {
        let in_overage = state.in_overage(&c.account_id, now, fallback_ms);
        let overage_bit = u8::from(pool.overage_policy == OveragePolicy::PreferPlan && in_overage);
        let group = group_of(c, in_overage);
        if kind_ordered {
            (group, c.priority, overage_bit)
        } else {
            (0, c.priority, group)
        }
    };
    let first = set.iter().map(|c| key_of(c)).min()?;
    let tier: Vec<&Candidate> = set.iter().copied().filter(|c| key_of(c) == first).collect();
    let chosen = match pool.strategy {
        Strategy::Pace => {
            // Remaining allowance over the hours to the earlier of the window's reset and the account's derived
            // end, so an account that runs out sooner is spent first; an api_key account scores by its worst
            // live bucket alone (a bucket refills within a minute, so Pace degrades to least utilized on it,
            // and the bucket's own reset never enters the score).
            let score = |c: &Candidate| -> f64 {
                let (remaining, reset) = remaining(state, &c.account_id, window, now);
                if snapshot.account(&c.account_id).map(|a| a.kind) == Some(AccountKind::ApiKey) {
                    return remaining;
                }
                let end = snapshot
                    .term(&c.account_id)
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
                remaining / hours
            };
            let best = tier
                .iter()
                .map(|c| score(c))
                .fold(f64::NEG_INFINITY, f64::max);
            let ties: Vec<&Candidate> = tier.iter().copied().filter(|c| score(c) == best).collect();
            Some(ties[rng.random_range(0..ties.len())])
        }
        Strategy::Priority => Some(tier[rng.random_range(0..tier.len())]),
        Strategy::RoundRobin => {
            // The cursor walks the tier's sorted ids, so an account's turn does not depend on the order
            // `resolve` happened to build the candidate list in.
            let mut cohort: Vec<&str> = tier.iter().map(|c| c.account_id.as_str()).collect();
            cohort.sort_unstable();
            let i = state.round_robin_next(&pool.id, first, &cohort, places);
            tier.iter().copied().find(|c| c.account_id == cohort[i])
        }
    };
    chosen.map(|c| (c, first))
}

/// The answer for a request nothing can serve, the pinned account's obstacle first when it has one. A fast rate
/// limit anywhere answers the earliest fast reset, since the client waits for the fast lane rather than for a
/// window it does not draw; a set whose every timed obstacle is a credits block answers the first one's reason,
/// which no wait clears before the poll reads credits again.
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
            Obstacle::Budget { .. } => Some(None),
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
            Obstacle::Budget { until } => Some((until, "overage".into(), "budget".into())),
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
