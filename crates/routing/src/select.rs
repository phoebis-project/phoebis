//! One selection: the spec's seven steps over the snapshot and the routing state.

use phoebis_store::{
    AccountKind, AccountRow, AccountState, OveragePolicy, PinRow, PoolRow, Strategy, SubStrategy,
};
use rand::{Rng, RngExt};

use crate::state::{PinRef, RoutingState, StateWrite, Tier};
use crate::{Candidate, RegistrySnapshot, ResolveError};

#[derive(Clone, Debug, PartialEq)]
pub struct SelectRequest {
    pub key_id: String,
    pub session_id: Option<String>,
    pub canonical_model: String,
    pub candidates: Vec<Candidate>,
    pub portable: bool,
    pub attempted: Vec<String>,
    pub now: i64,
    pub fallback_ms: i64,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrateReason {
    Overage,
    /// The pinned account's window for the request's model is spent while a sibling holds allowance in it.
    Spent,
    Utilization,
}

/// The hysteresis band under the migrate threshold: a destination counts only below `threshold - MIGRATE_BAND`.
pub const MIGRATE_BAND: f64 = 0.05;
/// How long the migrate rule leaves a pin alone after it moved for utilization.
pub const MIGRATE_DWELL_MS: i64 = 300_000;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exhaustion {
    Timed { deadline: i64, window: String },
    Untimed,
}

// Boxing the selection would put an allocation on the path every request takes, to shrink a value that is
// built once per selection and moved once into the answering channel.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum SelectResult {
    Exhausted(Exhaustion),
    Refused(ResolveError),
    Selected(Selection),
}

/// Why a candidate is out of the eligible set. Only a cooldown carries a deadline the caller can wait for.
enum Obstacle {
    Attempted,
    Cooldown { until: i64, window: String },
    Operator,
}

/// The window whose reset refunds an account's allowance for a model. The 5h window is not among them: its
/// expiry does not refund the plan, so an earlier 5h reset does not mean capacity comes back sooner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Window {
    Fable,
    SevenDay,
}

/// The classifier's name for the model-scoped weekly window, so an account excluded here dates an exhaustion
/// the way the 7d_oi rejection it anticipates would.
const FABLE_WINDOW: &str = "seven_day_fable";

fn window_of(canonical_model: &str) -> Window {
    if canonical_model.contains("fable") {
        Window::Fable
    } else {
        Window::SevenDay
    }
}

/// Whether an account can still serve a request for the model-scoped window. No sample, or a sample that never
/// read that window, counts as allowance, the way `least_utilized` reads an unsampled account as a 0.
///
/// A window read full excludes only while the reading stands for it: the reset the sample announced is the
/// moment the window refunds, and a reading that announced none is worth no more than the interval the router
/// re-reads on, or one full window would keep an account out until the process restarts.
fn holds_fable_allowance(state: &RoutingState, account_id: &str, now: i64) -> bool {
    let Some(sample) = state.sample(account_id) else {
        return true;
    };
    if sample.fable.unwrap_or(0.0) < 1.0 {
        return true;
    }
    match sample.fable_reset {
        Some(reset) => reset <= now,
        None => now - sample.observed_at >= state.sample_ttl_ms(),
    }
}

/// Whether an account can still serve a request whose allowance is judged in `window`: the model's own weekly
/// window for a fable request, the shared plan otherwise, either of whose two readings at 1.0 is spent. As
/// `holds_fable_allowance`: no sample, or one that never read the window, counts as allowance, and a window
/// read full excludes only while the reading stands for it, until the reset it named or, absent one, until the
/// reading ages out.
fn holds_allowance(state: &RoutingState, account_id: &str, window: Window, now: i64) -> bool {
    match window {
        Window::Fable => holds_fable_allowance(state, account_id, now),
        Window::SevenDay => {
            let Some(sample) = state.sample(account_id) else {
                return true;
            };
            // The shared plan has two readings and either one at 1.0 means the upstream refuses the request
            // until that window refunds; the `EarliestRefresh` arm reads the weekly reset for its own
            // ordering, this reads both for the one question of whether the account can serve at all.
            let standing = |reading: Option<f64>, reset: Option<i64>| {
                reading.unwrap_or(0.0) >= 1.0
                    && match reset {
                        Some(reset) => reset > now,
                        None => now - sample.observed_at < state.sample_ttl_ms(),
                    }
            };
            !standing(sample.five_hour, sample.five_hour_reset)
                && !standing(sample.seven_day, sample.seven_day_reset)
        }
    }
}

/// The pool's affinity in milliseconds, saturating for a value no clock reaches.
fn affinity_ttl_ms(pool: &PoolRow) -> i64 {
    i64::try_from(pool.affinity_ttl_seconds)
        .ok()
        .and_then(|s| s.checked_mul(1_000))
        .unwrap_or(i64::MAX)
}

fn state_name(state: AccountState) -> Option<&'static str> {
    match state {
        AccountState::Ready | AccountState::Refreshing => None,
        AccountState::Disconnected => Some("disconnected"),
        AccountState::FundingRequired => Some("funding_required"),
        AccountState::Pairing => Some("pairing"),
        AccountState::ReauthRequired => Some("reauth_required"),
        AccountState::Unpaired => Some("unpaired"),
    }
}

/// The facts a selection finds against a pinned account, in the order it evaluates them, for the event that
/// says why a pin was released without a migration: `expired` (idle past the pool's affinity TTL), `unserved`
/// (the account no longer serves the model for this key: it left the pool, the model was deselected, or the
/// key or the pool is gone), `deleted` (the account left the registry), `paused` (`enabled` is off), the
/// account's state when it cannot serve (`disconnected`, `funding_required`, `pairing`, `reauth_required`,
/// `unpaired`), `cooldown` (a live row blocks the model on the account), `overage` (in overage under a policy
/// other than `allow`), `attempted` (this request already tried it), and `spent` (no allowance left in the
/// request's window). Never empty: a pin released by a move whose destination then answered nothing carries
/// `released`, the one fact the list above does not evaluate.
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
    if state
        .cooldown(&pin.account_id, &request.canonical_model, request.now)
        .is_some()
    {
        reasons.push("cooldown");
    }
    if pool.is_some_and(|p| p.overage_policy != OveragePolicy::Allow)
        && state.in_overage(&pin.account_id)
    {
        reasons.push("overage");
    }
    if request.attempted.contains(&pin.account_id) {
        reasons.push("attempted");
    }
    if !holds_allowance(
        state,
        &pin.account_id,
        window_of(&request.canonical_model),
        request.now,
    ) {
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
    let session = request.session_id.as_deref().filter(|_| ttl_ms > 0);

    // Step 1: the pin, expired when idle past the TTL or when its account can no longer serve the model here. A
    // non-portable request keeps a pin whose account left the pool: the fence below answers for that account,
    // because no other account holds the state the request refers to.
    let mut pin = None;
    if let Some(session) = session {
        if let Some(current) = state
            .pin(&request.key_id, session, &request.canonical_model)
            .cloned()
        {
            let serves = request
                .candidates
                .iter()
                .any(|c| c.account_id == current.account_id);
            let idle = request.now - current.last_used_at > ttl_ms;
            if idle || (!serves && request.portable) {
                state.remove_pin(&request.key_id, session, &request.canonical_model);
                writes.push(StateWrite::DeletePin(
                    request.key_id.clone(),
                    session.to_string(),
                    request.canonical_model.clone(),
                ));
            } else {
                pin = Some(current);
            }
        }
    }

    // Step 2: the eligible set and the obstacle of everything outside it.
    let mut eligible: Vec<&Candidate> = Vec::new();
    let mut obstacles: Vec<Obstacle> = Vec::new();
    for candidate in &request.candidates {
        match obstacle(
            state,
            snapshot,
            pool.overage_policy,
            candidate,
            &request.canonical_model,
            &request.attempted,
            request.now,
            request.fallback_ms,
        ) {
            None => eligible.push(candidate),
            Some(o) => obstacles.push(o),
        }
    }
    let in_set = |set: &[&Candidate], id: &str| set.iter().any(|c| c.account_id == id);

    // Step 3: a non-portable request is bound to its pin.
    if let (false, Some(current)) = (request.portable, pin.as_ref()) {
        if in_set(&eligible, &current.account_id) {
            return (
                SelectResult::Selected(renew(
                    state,
                    snapshot,
                    current,
                    &request.candidates,
                    request.now,
                    &mut writes,
                )),
                writes,
            );
        }
        let own = obstacle(
            state,
            snapshot,
            pool.overage_policy,
            &Candidate {
                account_id: current.account_id.clone(),
                priority: 0,
                upstream_model: String::new(),
            },
            &request.canonical_model,
            &[],
            request.now,
            request.fallback_ms,
        );
        let exhaustion = match own {
            Some(Obstacle::Cooldown { until, window }) => Exhaustion::Timed {
                deadline: until,
                window,
            },
            _ => Exhaustion::Untimed,
        };
        return (SelectResult::Exhausted(exhaustion), writes);
    }

    let window = window_of(&request.canonical_model);

    // Step 4: the migrate rule, for a portable request with a live pin.
    let mut migrated = None;
    let mut selection_set: Vec<&Candidate> = eligible.clone();
    if let Some(current) = pin.as_ref() {
        let leaving_overage = pool.overage_policy == OveragePolicy::PreferPlan
            && state.in_overage(&current.account_id);
        let mut destination: Option<Vec<&Candidate>> = None;
        let mut reason = None;
        if leaving_overage {
            destination = Some(
                eligible
                    .iter()
                    .copied()
                    .filter(|c| !state.in_overage(&c.account_id))
                    .collect(),
            );
            reason = Some(MigrateReason::Overage);
        }
        if let Some(threshold) = pool.migrate_above_utilization {
            // A negative difference (a `migrated_at` ahead of the clock, from a backwards step) must not read
            // as dwelling forever, so the range excludes it rather than only bounding the top.
            let dwelling = current
                .migrated_at
                .is_some_and(|at| (0..MIGRATE_DWELL_MS).contains(&(request.now - at)));
            let hot = !dwelling
                && state.sample(&current.account_id).is_some_and(|s| {
                    s.five_hour.unwrap_or(0.0) >= threshold
                        || s.seven_day.unwrap_or(0.0) >= threshold
                });
            if hot {
                let cool: Vec<&Candidate> = eligible
                    .iter()
                    .copied()
                    .filter(|c| {
                        state.sample(&c.account_id).is_none_or(|s| {
                            s.five_hour.unwrap_or(0.0) < threshold - MIGRATE_BAND
                                && s.seven_day.unwrap_or(0.0) < threshold - MIGRATE_BAND
                        })
                    })
                    .collect();
                destination = Some(match destination {
                    Some(set) => set
                        .into_iter()
                        .filter(|c| cool.iter().any(|k| k.account_id == c.account_id))
                        .collect(),
                    None => cool,
                });
                reason = reason.or(Some(MigrateReason::Utilization));
            }
        }
        // A spent window is not a hover: the pinned account cannot serve the request until the window resets,
        // so the pin moves as soon as a sibling holds allowance, and the band and the dwell that keep two
        // accounts from trading a pin at the threshold do not apply. The set replaces the band's rather than
        // intersecting it (every account the band admits holds allowance, so this is never the narrower set).
        // Step 5 would otherwise renew the session on the spent account until the upstream's 429 moved it,
        // one request later than the sample already allows. Under `prefer_plan` a sibling in known overage is
        // no destination, whether or not this pin is itself leaving overage: the overage rule hands the
        // session back on the next request, the two rules trading it on no new evidence, so a spent pin with
        // only such siblings stays and the upstream's 429 decides, as with no sibling at all.
        if !holds_allowance(state, &current.account_id, window, request.now) {
            let with_allowance: Vec<&Candidate> = eligible
                .iter()
                .copied()
                .filter(|c| {
                    c.account_id != current.account_id
                        && holds_allowance(state, &c.account_id, window, request.now)
                        && !(pool.overage_policy == OveragePolicy::PreferPlan
                            && state.in_overage(&c.account_id))
                })
                .collect();
            if !with_allowance.is_empty() {
                destination = Some(with_allowance);
                if reason != Some(MigrateReason::Overage) {
                    reason = Some(MigrateReason::Spent);
                }
            }
        }
        // An account with no allowance left in the model's own window is no destination: step 6 drops it, so
        // releasing the pin for a set of them would strand the request on nothing.
        if window == Window::Fable {
            destination = destination.map(|set| {
                set.into_iter()
                    .filter(|c| holds_fable_allowance(state, &c.account_id, request.now))
                    .collect()
            });
        }
        if let Some(set) = destination.filter(|set| !set.is_empty()) {
            state.remove_pin(&current.key_id, &current.session_id, &current.model);
            writes.push(StateWrite::DeletePin(
                current.key_id.clone(),
                current.session_id.clone(),
                current.model.clone(),
            ));
            migrated = reason;
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
                    current,
                    &request.candidates,
                    request.now,
                    &mut writes,
                )),
                writes,
            );
        }
    }

    // Step 6: a model with its own window needs allowance left in it, the depletion threshold prefers the
    // accounts under it, then tiers and the strategy inside the first non-empty one.
    if window == Window::Fable {
        let (left, spent): (Vec<&Candidate>, Vec<&Candidate>) = selection_set
            .iter()
            .copied()
            .partition(|c| holds_fable_allowance(state, &c.account_id, request.now));
        obstacles.extend(spent.iter().filter_map(|c| {
            state
                .sample(&c.account_id)
                .and_then(|s| s.fable_reset)
                .map(|until| Obstacle::Cooldown {
                    until,
                    window: FABLE_WINDOW.to_string(),
                })
        }));
        selection_set = left;
    }
    // The threshold is a preference, not a refusal: with every account above it the request goes to the least
    // bad one rather than answering exhausted while capacity remains.
    if let Some(threshold) = pool.select_below_utilization {
        let below: Vec<&Candidate> = selection_set
            .iter()
            .copied()
            .filter(|c| {
                state
                    .sample(&c.account_id)
                    .is_none_or(|s| s.five_hour.unwrap_or(0.0) < threshold)
            })
            .collect();
        if !below.is_empty() {
            selection_set = below;
        }
    }
    let Some(chosen) = choose(state, snapshot, pool, window, &selection_set, rng) else {
        return (SelectResult::Exhausted(exhaustion_of(obstacles)), writes);
    };

    // Step 7: a fresh pin with a fresh epoch.
    let pin_ref = session.map(|session| {
        let epoch = state.fresh_epoch();
        let row = PinRow {
            key_id: request.key_id.clone(),
            session_id: session.to_string(),
            model: request.canonical_model.clone(),
            account_id: chosen.account_id.clone(),
            epoch,
            created_at: request.now,
            last_used_at: request.now,
            migrated_at: (migrated == Some(MigrateReason::Utilization)).then_some(request.now),
        };
        state.put_pin(row.clone());
        writes.push(StateWrite::UpsertPin(row));
        PinRef {
            key_id: request.key_id.clone(),
            session_id: session.to_string(),
            model: request.canonical_model.clone(),
            epoch,
        }
    });
    (
        SelectResult::Selected(Selection {
            account: selected_row(snapshot, &chosen.account_id),
            upstream_model: chosen.upstream_model.clone(),
            pin: pin_ref,
            migrated,
            in_overage: state.in_overage(&chosen.account_id),
        }),
        writes,
    )
}

#[allow(clippy::too_many_arguments)]
fn obstacle(
    state: &RoutingState,
    snapshot: &RegistrySnapshot,
    policy: OveragePolicy,
    candidate: &Candidate,
    model: &str,
    attempted: &[String],
    now: i64,
    fallback_ms: i64,
) -> Option<Obstacle> {
    let tried = attempted.iter().any(|a| a == &candidate.account_id);
    // A candidate the registry no longer holds was deleted after admission; it is nobody's to select, and a
    // cooldown row it left behind must not date an exhaustion no wait can clear.
    let Some(account) = snapshot.account(&candidate.account_id) else {
        return Some(Obstacle::Operator);
    };
    if !account.enabled
        || !matches!(
            account.state,
            AccountState::Ready | AccountState::Refreshing
        )
    {
        return Some(Obstacle::Operator);
    }
    if let Some(cooldown) = state.cooldown(&candidate.account_id, model, now) {
        return Some(Obstacle::Cooldown {
            until: cooldown.until,
            window: cooldown.window.clone(),
        });
    }
    if policy == OveragePolicy::Never {
        if let Some(deadline) = state
            .overage_deadline(&candidate.account_id, fallback_ms)
            .filter(|d| *d > now)
        {
            return Some(Obstacle::Cooldown {
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

fn renew(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    current: &PinRow,
    candidates: &[Candidate],
    now: i64,
    writes: &mut Vec<StateWrite>,
) -> Selection {
    let renewed = PinRow {
        last_used_at: now,
        ..current.clone()
    };
    state.put_pin(renewed.clone());
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
            model: current.model.clone(),
            epoch: current.epoch,
        }),
        migrated: None,
        in_overage: state.in_overage(&current.account_id),
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

fn choose<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: Window,
    set: &[&'a Candidate],
    rng: &mut impl Rng,
) -> Option<&'a Candidate> {
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
        let in_overage = state.in_overage(&c.account_id);
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
    // The sub-strategy exists because a sample-driven strategy has no sample to order an api_key account by. An
    // operator who chose round_robin or priority_random chose it for the whole pool, so group three takes that
    // strategy and the stored sub-strategy waits for the day the pool goes sample-driven.
    let sample_driven = matches!(
        pool.strategy,
        Strategy::EarliestRefresh | Strategy::LeastUtilized
    );
    let strategy = if kind_ordered && first.0 == 2 && sample_driven {
        match pool.api_key_sub_strategy {
            SubStrategy::PriorityRandom => Strategy::PriorityRandom,
            SubStrategy::RoundRobin => Strategy::RoundRobin,
        }
    } else {
        pool.strategy
    };
    match strategy {
        Strategy::EarliestRefresh => {
            let reset = |c: &Candidate| {
                state.sample(&c.account_id).and_then(|s| match window {
                    Window::Fable => s.fable_reset,
                    Window::SevenDay => s.seven_day_reset,
                })
            };
            // A reset nobody has sampled is a reset infinitely far off, which puts the account holding it
            // behind every account whose refund is dated.
            let earliest = tier.iter().filter_map(|c| reset(c)).min();
            let ties: Vec<&Candidate> = tier
                .iter()
                .copied()
                .filter(|c| reset(c) == earliest)
                .collect();
            Some(ties[rng.random_range(0..ties.len())])
        }
        Strategy::LeastUtilized => {
            let utilization = |c: &Candidate| {
                state
                    .sample(&c.account_id)
                    .and_then(|s| s.five_hour)
                    .unwrap_or(0.0)
            };
            let lowest = tier
                .iter()
                .map(|c| utilization(c))
                .fold(f64::INFINITY, f64::min);
            let mut ties: Vec<&Candidate> = tier
                .iter()
                .copied()
                .filter(|c| utilization(c) == lowest)
                .collect();
            if ties.is_empty() {
                ties = tier;
            }
            Some(ties[rng.random_range(0..ties.len())])
        }
        Strategy::PriorityRandom => Some(tier[rng.random_range(0..tier.len())]),
        Strategy::RoundRobin => {
            // The cursor walks the tier's sorted ids, so an account's turn does not depend on the order
            // `resolve` happened to build the candidate list in.
            let mut cohort: Vec<&str> = tier.iter().map(|c| c.account_id.as_str()).collect();
            cohort.sort_unstable();
            let i = state.round_robin_next(&pool.id, first, &cohort);
            tier.iter().copied().find(|c| c.account_id == cohort[i])
        }
    }
}

fn exhaustion_of(obstacles: Vec<Obstacle>) -> Exhaustion {
    obstacles
        .into_iter()
        .filter_map(|o| match o {
            Obstacle::Cooldown { until, window } => Some((until, window)),
            _ => None,
        })
        .min_by_key(|(until, _)| *until)
        .map(|(deadline, window)| Exhaustion::Timed { deadline, window })
        .unwrap_or(Exhaustion::Untimed)
}
