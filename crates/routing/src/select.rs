//! One selection: the spec's seven steps over the snapshot and the routing state.

use phoebis_store::{AccountKind, AccountRow, AccountState, OveragePolicy, PinRow, PoolRow, Strategy};
use rand::{Rng, RngExt};

use crate::state::{PinRef, RoutingState, StateWrite, Tier, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use crate::windows::window_for;
use crate::{term, Candidate, RegistrySnapshot, ResolveError};

#[derive(Clone, Debug, PartialEq)]
pub struct SelectRequest {
    pub key_id: String,
    /// The authenticated key's `created_at` at admission. The id is reusable, so the lifetime is the identity: a
    /// stale-revision re-authentication that finds another generation under the id refuses the request.
    pub key_generation: i64,
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

impl MigrateReason {
    pub fn as_str(self) -> &'static str {
        match self {
            MigrateReason::Overage => "overage",
            MigrateReason::Spent => "spent",
            MigrateReason::Utilization => "utilization",
        }
    }
}

/// The hysteresis band under the migrate threshold: a destination counts only below `threshold - MIGRATE_BAND`.
pub const MIGRATE_BAND: f64 = 0.05;

/// Why a selection landed where it did, for the attempt row and the request drawer. `Serialize` here, so the
/// relay can put the trace on the bus as JSON without this crate's callers needing to depend on the ledger.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct SelectionTrace {
    pub strategy: &'static str,
    /// The tier the choice was made in (group, priority, overage bit) as a three-element array; `None` for a
    /// renewed pin.
    pub tier: Option<Tier>,
    pub renewed: bool,
    pub moved: Option<&'static str>,
    /// Every candidate the choice passed over.
    pub skipped: Vec<Skipped>,
}

/// One candidate a choice passed over, and why; serializes as `{"account_id": ..., "obstacle": ...}` so the
/// console decodes a named object, not a tuple.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Skipped {
    pub account_id: String,
    /// A step-2 obstacle (`attempted`, `cooldown`, `operator`, `overage`, `subscription_ended`), or a
    /// partition's name: `above_ceiling`, `at_cap`, `floor`, `spent`, `winding_down`, or `ranked` for an
    /// eligible sibling the strategy did not pick.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exhaustion {
    /// No candidate is eligible, and every cooldown in the way is a rejection of the request's model.
    ModelRejected,
    Timed {
        deadline: i64,
        window: String,
    },
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
    Cooldown {
        reason: String,
        until: i64,
        window: String,
    },
    /// Cancelled and past the saved end.
    Ended,
    Operator,
}

/// The step-2 name `release_reasons` and the trace report for an obstacle: `attempted`, `cooldown`, `operator`,
/// `overage`, or `subscription_ended`.
fn obstacle_name(obstacle: &Obstacle) -> &'static str {
    match obstacle {
        Obstacle::Attempted => "attempted",
        Obstacle::Cooldown { reason, .. } if reason == "overage" => "overage",
        Obstacle::Cooldown { .. } => "cooldown",
        Obstacle::Ended => "subscription_ended",
        Obstacle::Operator => "operator",
    }
}

/// Whether an account can still serve a request whose allowance is judged in `window`: the model's own weekly
/// window for a fable request, both shared plan windows otherwise, either of whose two readings at 1.0 is
/// spent. No sample, or a sample that never read the window, counts as allowance (an api_key account, whose
/// sample carries no plan window of its own, always holds it here), and a window read full excludes only while
/// the reading stands for it, through `WindowReading::live_at`.
fn holds_allowance(state: &RoutingState, account_id: &str, window: &str, now: i64) -> bool {
    let Some(sample) = state.sample(account_id) else {
        return true;
    };
    let ttl = state.sample_ttl_ms();
    let spent = |name: &str| {
        sample
            .reading(name)
            .is_some_and(|r| r.utilization >= 1.0 && r.live_at(now, ttl))
    };
    if window == SEVEN_DAY_FABLE {
        !spent(SEVEN_DAY_FABLE)
    } else {
        !spent(FIVE_HOUR) && !spent(SEVEN_DAY)
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
/// account), `overage` (in overage under a policy other than `allow`), `attempted` (this request already tried
/// it), and `spent` (no allowance left in the request's window). Never empty: a pin released by a move whose
/// destination then answered nothing carries `released`, the one fact the list above does not evaluate.
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
    if state
        .cooldown(&pin.account_id, &request.canonical_model, request.now)
        .is_some()
    {
        reasons.push("cooldown");
    }
    if pool.is_some_and(|p| p.overage_policy != OveragePolicy::Allow)
        && state.in_overage(&pin.account_id, request.now, request.fallback_ms)
    {
        reasons.push("overage");
    }
    if request.attempted.contains(&pin.account_id) {
        reasons.push("attempted");
    }
    if !holds_allowance(
        state,
        &pin.account_id,
        window_for(&request.canonical_model),
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

    // Step 2: the eligible set and the obstacle of everything outside it, each kept with its account id for
    // the release event and the trace.
    let mut eligible: Vec<&Candidate> = Vec::new();
    let mut obstacles: Vec<(String, Obstacle)> = Vec::new();
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
            Some(o) => obstacles.push((candidate.account_id.clone(), o)),
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
                    request.fallback_ms,
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
                incarnation: current.incarnation,
                priority: 0,
                upstream_model: String::new(),
            },
            &request.canonical_model,
            &[],
            request.now,
            request.fallback_ms,
        );
        let exhaustion = match own {
            Some(Obstacle::Cooldown { reason, .. }) if reason == MODEL_REJECTED_REASON => {
                Exhaustion::ModelRejected
            }
            Some(Obstacle::Cooldown { until, window, .. }) => Exhaustion::Timed {
                deadline: until,
                window,
            },
            _ => Exhaustion::Untimed,
        };
        return (SelectResult::Exhausted(exhaustion), writes);
    }

    let window = window_for(&request.canonical_model);

    // Step 4: the migrate rule, for a portable request with a live pin.
    let mut migrated = None;
    let mut selection_set: Vec<&Candidate> = eligible.clone();
    if let Some(current) = pin.as_ref() {
        let leaving_overage = pool.overage_policy == OveragePolicy::PreferPlan
            && state.in_overage(&current.account_id, request.now, request.fallback_ms);
        let mut destination: Option<Vec<&Candidate>> = None;
        let mut reason = None;
        if leaving_overage {
            destination = Some(
                eligible
                    .iter()
                    .copied()
                    .filter(|c| !state.in_overage(&c.account_id, request.now, request.fallback_ms))
                    .collect(),
            );
            reason = Some(MigrateReason::Overage);
        }
        if let Some(threshold) = pool.migrate_above_utilization {
            let reading_at_least = |account_id: &str, name: &str, threshold: f64| {
                state
                    .sample(account_id)
                    .and_then(|s| s.reading(name))
                    .is_some_and(|r| r.utilization >= threshold)
            };
            let reading_under = |account_id: &str, name: &str, threshold: f64| {
                state
                    .sample(account_id)
                    .and_then(|s| s.reading(name))
                    .is_none_or(|r| r.utilization < threshold)
            };
            let hot = reading_at_least(&current.account_id, FIVE_HOUR, threshold)
                || reading_at_least(&current.account_id, SEVEN_DAY, threshold);
            if hot {
                let cool: Vec<&Candidate> = eligible
                    .iter()
                    .copied()
                    .filter(|c| {
                        reading_under(&c.account_id, FIVE_HOUR, threshold - MIGRATE_BAND)
                            && reading_under(&c.account_id, SEVEN_DAY, threshold - MIGRATE_BAND)
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
                            && state.in_overage(&c.account_id, request.now, request.fallback_ms))
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
        if window == SEVEN_DAY_FABLE {
            destination = destination.map(|set| {
                set.into_iter()
                    .filter(|c| holds_allowance(state, &c.account_id, window, request.now))
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
                    request.fallback_ms,
                    &mut writes,
                )),
                writes,
            );
        }
    }

    // Step 6: a model with its own window needs allowance left in it, the ceiling prefers the accounts under
    // it, the cap and the wind-down prefer accounts that can take a fresh session, and under Pace the floor
    // keeps an almost-spent account as the last resort. Each partition falls through to the wider set it
    // narrowed when it would otherwise empty the selection, and every candidate a partition drops joins the
    // trace under that partition's name.
    let mut skipped: Vec<Skipped> = Vec::new();
    // A dated deadline for the accounts the Fable partition drops, kept apart from `skipped`: it answers
    // `exhaustion_of` when nothing is left to choose from, but every dropped candidate joins the trace as
    // `spent` whether or not its reading names a reset, so the deadline never doubles as its own trace label.
    let mut spent_deadlines: Vec<(String, Obstacle)> = Vec::new();
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
                .sample(&c.account_id)
                .and_then(|s| s.reading(SEVEN_DAY_FABLE))
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
    // The threshold is a preference, not a refusal: with every account above it the request goes to the least
    // bad one rather than answering exhausted while capacity remains.
    if let Some(threshold) = pool.select_below_utilization {
        let below: Vec<&Candidate> = selection_set
            .iter()
            .copied()
            .filter(|c| {
                state
                    .sample(&c.account_id)
                    .and_then(|s| s.reading(FIVE_HOUR))
                    .is_none_or(|r| r.utilization < threshold)
            })
            .collect();
        narrow(&mut selection_set, &mut skipped, below, "above_ceiling");
    }
    // An account at its cap is no place to put a session.
    let under_cap: Vec<&Candidate> = selection_set
        .iter()
        .copied()
        .filter(|c| state.in_flight().account(&c.account_id) < pool.failover.in_flight_cap)
        .collect();
    narrow(&mut selection_set, &mut skipped, under_cap, "at_cap");
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
    let Some((chosen, tier)) = choose(
        state,
        snapshot,
        pool,
        window,
        &selection_set,
        request.now,
        request.fallback_ms,
        rng,
    ) else {
        let obstacles = obstacles
            .into_iter()
            .chain(spent_deadlines)
            .map(|(_, o)| o)
            .collect();
        return (SelectResult::Exhausted(exhaustion_of(obstacles)), writes);
    };
    for c in &selection_set {
        if c.account_id != chosen.account_id {
            skipped.push(Skipped {
                account_id: c.account_id.clone(),
                obstacle: "ranked",
            });
        }
    }

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
            // The generation the relay admitted this request under, not the snapshot's: a key deleted and
            // created again under its id between admission and here must not stamp the new holder's
            // generation on a pin this request's old one is choosing.
            key_generation: request.key_generation,
            incarnation: chosen.incarnation,
            burn: 0.0,
            burn_at: None,
            last_input_tokens: 0,
            last_refresh_at: request.now,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
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
    let mut trace_skipped: Vec<Skipped> = obstacles
        .iter()
        .map(|(account_id, o)| Skipped {
            account_id: account_id.clone(),
            obstacle: obstacle_name(o),
        })
        .collect();
    trace_skipped.extend(skipped);
    (
        SelectResult::Selected(Selection {
            account: selected_row(snapshot, &chosen.account_id),
            upstream_model: chosen.upstream_model.clone(),
            pin: pin_ref,
            migrated,
            in_overage: state.in_overage(&chosen.account_id, request.now, request.fallback_ms),
            trace: SelectionTrace {
                strategy: pool.strategy.as_str(),
                tier: Some(tier),
                renewed: false,
                moved: migrated.map(MigrateReason::as_str),
                skipped: trace_skipped,
            },
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
    if !account.enabled || account.state != AccountState::Ready {
        return Some(Obstacle::Operator);
    }
    if snapshot
        .term(&candidate.account_id)
        .is_some_and(|term| term::ended(term, now))
    {
        return Some(Obstacle::Ended);
    }
    if let Some(cooldown) = state.cooldown(&candidate.account_id, model, now) {
        return Some(Obstacle::Cooldown {
            reason: cooldown.reason.clone(),
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

#[allow(clippy::too_many_arguments)]
fn renew(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    current: &PinRow,
    candidates: &[Candidate],
    now: i64,
    fallback_ms: i64,
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
        in_overage: state.in_overage(&current.account_id, now, fallback_ms),
        trace: SelectionTrace {
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

const WEEK_HOURS: f64 = 168.0;

/// Pace's remaining allowance for a candidate at `now`, and the live reading's reset when it came from the
/// candidate's own `window` (an api_key account's worst-bucket reset never enters the score). A subscription
/// account whose window was refunded or aged out, an api_key account with every bucket refunded or aged out, or
/// an account never sampled, all read as untouched: remaining 1, no reset.
fn remaining(state: &RoutingState, account_id: &str, window: &str, now: i64) -> (f64, Option<i64>) {
    let ttl = state.sample_ttl_ms();
    let Some(sample) = state.sample(account_id) else {
        return (1.0, None);
    };
    if let Some(reading) = sample.reading(window).filter(|r| r.live_at(now, ttl)) {
        return ((1.0 - reading.utilization).max(0.0), reading.reset);
    }
    match sample.worst_bucket(now, ttl) {
        Some(reading) => ((1.0 - reading.utilization).max(0.0), None),
        None => (1.0, None),
    }
}

#[allow(clippy::too_many_arguments)]
fn choose<'a>(
    state: &mut RoutingState,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    window: &str,
    set: &[&'a Candidate],
    now: i64,
    fallback_ms: i64,
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
            let i = state.round_robin_next(&pool.id, first, &cohort);
            tier.iter().copied().find(|c| c.account_id == cohort[i])
        }
    };
    chosen.map(|c| (c, first))
}

fn exhaustion_of(obstacles: Vec<Obstacle>) -> Exhaustion {
    let cooldowns: Vec<(i64, String, String)> = obstacles
        .into_iter()
        .filter_map(|o| match o {
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
