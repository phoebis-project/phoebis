//! One selection: the spec's seven steps over the snapshot and the routing state.

use phoebis_store::{AccountRow, AccountState, OveragePolicy, PinRow, Strategy};
use rand::{Rng, RngExt};

use crate::state::{PinRef, RoutingState, StateWrite};
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
    Utilization,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    /// The row the selection was made against, so what a request dispatches with is the endpoint and the
    /// credential one snapshot held together rather than two lookups taken moments apart.
    pub account: AccountRow,
    pub upstream_model: String,
    pub pin: Option<PinRef>,
    pub migrated: Option<MigrateReason>,
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
    let ttl_ms = i64::try_from(pool.affinity_ttl_seconds)
        .ok()
        .and_then(|s| s.checked_mul(1_000))
        .unwrap_or(i64::MAX);
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
        let mut destination: Option<Vec<&Candidate>> = None;
        let mut reason = None;
        if pool.overage_policy == OveragePolicy::PreferPlan && state.in_overage(&current.account_id)
        {
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
            let hot = state.sample(&current.account_id).is_some_and(|s| {
                s.five_hour.unwrap_or(0.0) >= threshold || s.seven_day.unwrap_or(0.0) >= threshold
            });
            if hot {
                let cool: Vec<&Candidate> = eligible
                    .iter()
                    .copied()
                    .filter(|c| {
                        state.sample(&c.account_id).is_none_or(|s| {
                            s.five_hour.unwrap_or(0.0) < threshold
                                && s.seven_day.unwrap_or(0.0) < threshold
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
    let Some(chosen) = choose(
        state,
        pool.strategy,
        pool.overage_policy,
        window,
        &selection_set,
        rng,
    ) else {
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
    state: &RoutingState,
    strategy: Strategy,
    policy: OveragePolicy,
    window: Window,
    set: &[&'a Candidate],
    rng: &mut impl Rng,
) -> Option<&'a Candidate> {
    let tier_of = |c: &Candidate| {
        (
            c.priority,
            u8::from(policy == OveragePolicy::PreferPlan && state.in_overage(&c.account_id)),
        )
    };
    let first = set.iter().map(|c| tier_of(c)).min()?;
    let tier: Vec<&Candidate> = set
        .iter()
        .copied()
        .filter(|c| tier_of(c) == first)
        .collect();
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
