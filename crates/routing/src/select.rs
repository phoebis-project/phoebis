//! One selection: the spec's seven steps over the snapshot and the routing state.

use phoebis_store::{AccountState, OveragePolicy, PinRow, Strategy};
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
    pub account_id: String,
    pub upstream_model: String,
    pub pin: Option<PinRef>,
    pub migrated: Option<MigrateReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exhaustion {
    Timed { deadline: i64, window: String },
    Untimed,
}

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
    let ttl_ms = pool.affinity_ttl_seconds as i64 * 1_000;
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
                    current,
                    &request.candidates,
                    request.now,
                    &mut writes,
                )),
                writes,
            );
        }
    }

    // Step 6: tiers, then the strategy inside the first non-empty one.
    let Some(chosen) = choose(
        state,
        pool.strategy,
        pool.overage_policy,
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
            account_id: chosen.account_id.clone(),
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
    if attempted.iter().any(|a| a == &candidate.account_id) {
        return Some(Obstacle::Attempted);
    }
    // A candidate the registry no longer holds was deleted after admission; it is nobody's to select.
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
    None
}

fn renew(
    state: &mut RoutingState,
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
        account_id: current.account_id.clone(),
        // Step 1 checked that the pinned account still serves the model, so its spelling is among the candidates.
        upstream_model: candidates
            .iter()
            .find(|c| c.account_id == current.account_id)
            .map(|c| c.upstream_model.clone())
            .unwrap_or_default(),
        pin: Some(PinRef {
            key_id: current.key_id.clone(),
            session_id: current.session_id.clone(),
            model: current.model.clone(),
            epoch: current.epoch,
        }),
        migrated: None,
    }
}

fn choose<'a>(
    state: &RoutingState,
    strategy: Strategy,
    policy: OveragePolicy,
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
        Strategy::PriorityRandom => Some(tier[rng.random_range(0..tier.len())]),
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
            let ties: Vec<&Candidate> = tier
                .into_iter()
                .filter(|c| utilization(c) == lowest)
                .collect();
            Some(ties[rng.random_range(0..ties.len())])
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
