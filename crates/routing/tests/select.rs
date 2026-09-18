use phoebis_routing::{
    resolve, select, EvidenceSource, Exhaustion, MigrateReason, OverageEvidence, PinRef,
    RegistrySnapshot, RoutingState, SelectRequest, SelectResult, StateWrite, UtilizationSample,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, ModelSource, OveragePolicy,
    PoolMember, PoolRow, RegistryLoad, RoutingLoad, ServedModel, Strategy, SubStrategy,
};
use rand::rngs::StdRng;
use rand::SeedableRng;

const HOUR: i64 = 3_600_000;

struct Pool {
    policy: OveragePolicy,
    strategy: Strategy,
    affinity: u64,
    migrate: Option<f64>,
    select_below: Option<f64>,
    members: Vec<(&'static str, u32)>,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            policy: OveragePolicy::PreferPlan,
            strategy: Strategy::PriorityRandom,
            affinity: 3600,
            migrate: None,
            select_below: None,
            members: vec![("a", 0), ("b", 0), ("c", 0)],
        }
    }
}

fn snapshot(pool: Pool) -> RegistrySnapshot {
    snapshot_with(pool, &[])
}

/// `unavailable` names accounts inserted disabled or in a state the eligible set excludes.
fn snapshot_with(pool: Pool, unavailable: &[(&str, bool, AccountState)]) -> RegistrySnapshot {
    let account = |id: &str| {
        let (enabled, state) = unavailable
            .iter()
            .find(|(u, _, _)| *u == id)
            .map(|(_, e, s)| (*e, *s))
            .unwrap_or((true, AccountState::Ready));
        AccountRow {
            id: id.into(),
            kind: AccountKind::Oauth,
            label: id.into(),
            enabled,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state,
        }
    };
    let served = |a: &str, m: &str| ServedModel {
        account_id: a.into(),
        upstream_id: m.into(),
        canonical_id: m.into(),
        selected: true,
        source: ModelSource::Discovered,
        long_context: false,
        missing: false,
        is_new: false,
    };
    let load = RegistryLoad {
        accounts: ["a", "b", "c"].map(account).to_vec(),
        credential_generations: Vec::new(),
        keys: vec![ApiKeyRow {
            id: "k".into(),
            label: "k".into(),
            notes: None,
            pool_id: "p".into(),
            enabled: true,
            revoked_at: None,
            expires_at: None,
            allowed_models: None,
            keepalive_opt_out: false,
            body_capture_hours: None,
            secret_digest: vec![0; 32],
            prefix: "00000000".into(),
            created_at: 0,
            rotated_at: None,
            last_used_at: None,
        }],
        members: pool
            .members
            .iter()
            .map(|(a, p)| PoolMember {
                pool_id: "p".into(),
                account_id: (*a).into(),
                priority: *p,
            })
            .collect(),
        models: ["a", "b", "c"]
            .iter()
            .flat_map(|a| {
                [
                    served(a, "claude-fable-5-1"),
                    served(a, "claude-opus-5"),
                    served(a, "claude-sonnet-5"),
                ]
            })
            .collect(),
        pools: vec![PoolRow {
            id: "p".into(),
            label: "p".into(),
            enabled: true,
            strategy: pool.strategy,
            affinity_ttl_seconds: pool.affinity,
            overage_policy: pool.policy,
            migrate_above_utilization: pool.migrate,
            select_below_utilization: pool.select_below,
            default_allowed_models: None,
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: false,
            api_key_sub_strategy: SubStrategy::PriorityRandom,
            created_at: 0,
            updated_at: 0,
        }],
    };
    RegistrySnapshot::from_load(load, [0; 32], 0, 1)
}

fn request(
    s: &RegistrySnapshot,
    session: Option<&str>,
    model: &str,
    portable: bool,
    attempted: &[&str],
    now: i64,
) -> SelectRequest {
    SelectRequest {
        key_id: "k".into(),
        session_id: session.map(String::from),
        canonical_model: model.into(),
        candidates: resolve(s, s.key("k").unwrap(), model).unwrap(),
        portable,
        attempted: attempted.iter().map(|a| a.to_string()).collect(),
        now,
        fallback_ms: 60_000,
        revision: s.revision,
    }
}

fn cooled(account: &str, scope: &str, until: i64, window: &str) -> CooldownRow {
    CooldownRow {
        account_id: account.into(),
        scope: scope.into(),
        until,
        set_at: 0,
        reason: "rate_limit".into(),
        window: window.into(),
        source_request_id: None,
    }
}

fn overage(in_use: bool, observed_at: i64, reset: Option<i64>) -> OverageEvidence {
    OverageEvidence {
        in_use,
        since: Some(observed_at),
        reset,
        observed_at,
        source: EvidenceSource::Head,
    }
}

fn sample(five_hour: f64, seven_day: f64, observed_at: i64) -> UtilizationSample {
    UtilizationSample {
        fable: None,
        fable_reset: None,
        five_hour: Some(five_hour),
        five_hour_reset: None,
        seven_day: Some(seven_day),
        seven_day_reset: None,
        observed_at,
    }
}

/// The window resets the refresh order reads, over the utilization `sample` already carries.
fn refreshing(
    five_hour: f64,
    five_hour_reset: i64,
    seven_day_reset: i64,
    observed_at: i64,
) -> UtilizationSample {
    UtilizationSample {
        five_hour_reset: Some(five_hour_reset),
        seven_day_reset: Some(seven_day_reset),
        ..sample(five_hour, 0.1, observed_at)
    }
}

/// The fable allowance and its reset over a sample that already carries the shared windows.
fn with_fable(base: UtilizationSample, fable: f64, fable_reset: Option<i64>) -> UtilizationSample {
    UtilizationSample {
        fable: Some(fable),
        fable_reset,
        ..base
    }
}

fn picked(result: &SelectResult) -> &str {
    match result {
        SelectResult::Selected(s) => &s.account.id,
        other => panic!("expected a selection, got {other:?}"),
    }
}

fn rng() -> StdRng {
    StdRng::seed_from_u64(7)
}

#[test]
fn priority_zero_wins_until_every_priority_zero_account_is_ineligible_and_a_pin_stays_after_recovery(
) {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut r
            )
            .0
        ),
        "a"
    );
    st.record_cooldown(cooled("a", "account", 1_000 + HOUR, "five_hour"));
    let (result, writes) = select(
        &mut st,
        &s,
        &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &mut r,
    );
    assert_eq!(
        picked(&result),
        "b",
        "priority 1 only while every priority 0 account is cooled"
    );
    assert!(writes.iter().any(
        |w| matches!(w, StateWrite::UpsertPin(p) if p.account_id == "b" && p.session_id == "s2")
    ));
    st.clear_cooldown("a", "account");
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 3_000),
                &mut r
            )
            .0
        ),
        "b",
        "stickiness beats priority after recovery"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s3"), "claude-sonnet-5", true, &[], 3_000),
                &mut r
            )
            .0
        ),
        "a",
        "a new session goes back to priority 0"
    );
}

#[test]
fn least_utilized_picks_the_lowest_five_hour_sample_and_random_without_samples() {
    let s = snapshot(Pool {
        strategy: Strategy::LeastUtilized,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", sample(0.9, 0.1, 1));
    st.record_sample("b", sample(0.2, 0.1, 1));
    st.record_sample("c", sample(0.5, 0.1, 1));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng()
            )
            .0
        ),
        "b"
    );
    let mut none = RoutingState::from_load(RoutingLoad::default());
    let mut seen = std::collections::HashSet::new();
    for i in 0..40 {
        let (result, _) = select(
            &mut none,
            &s,
            &request(&s, None, "claude-sonnet-5", true, &[], i),
            &mut StdRng::seed_from_u64(i as u64),
        );
        seen.insert(picked(&result).to_string());
    }
    assert_eq!(
        seen.len(),
        3,
        "without samples every account is a 0 and the tie is random"
    );
}

#[test]
fn earliest_refresh_orders_by_the_seven_day_reset_and_sorts_an_account_without_one_last() {
    let s = snapshot(Pool {
        strategy: Strategy::EarliestRefresh,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // `a` clears its 5h window first and refreshes its plan last; only the second of those orders.
    st.record_sample("a", refreshing(0.1, HOUR, 5 * HOUR, 1));
    st.record_sample("b", refreshing(0.9, 9 * HOUR, 2 * HOUR, 1));
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    for seed in 0..8u64 {
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
            "b",
            "the 7d window that refunds soonest wins, whatever the 5h resets say"
        );
    }
    st.record_sample("b", refreshing(0.9, 9 * HOUR, 6 * HOUR, 2));
    for seed in 0..8u64 {
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
            "a",
            "and `c`, never sampled, is behind both: an unknown reset is infinitely far"
        );
    }
    let mut none = RoutingState::from_load(RoutingLoad::default());
    let mut seen = std::collections::HashSet::new();
    for i in 0..40 {
        let (result, _) = select(
            &mut none,
            &s,
            &request(&s, None, "claude-sonnet-5", true, &[], i),
            &mut StdRng::seed_from_u64(i as u64),
        );
        seen.insert(picked(&result).to_string());
    }
    assert_eq!(
        seen.len(),
        3,
        "with no reset anywhere every account is equally far off and the tie is random"
    );
}

#[test]
fn a_fable_request_skips_a_spent_fable_window_under_every_strategy_and_orders_by_its_reset() {
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // `a` would win under all three rules on the shared windows alone: the lowest 5h and the earliest 7d.
    st.record_sample(
        "a",
        with_fable(refreshing(0.1, HOUR, HOUR, 1), 1.0, Some(4 * HOUR)),
    );
    st.record_sample(
        "b",
        with_fable(refreshing(0.5, HOUR, 2 * HOUR, 1), 0.5, Some(9 * HOUR)),
    );
    st.record_sample(
        "c",
        with_fable(refreshing(0.6, HOUR, 8 * HOUR, 1), 0.2, Some(3 * HOUR)),
    );
    for strategy in [
        Strategy::EarliestRefresh,
        Strategy::LeastUtilized,
        Strategy::PriorityRandom,
    ] {
        let s = snapshot(Pool {
            strategy,
            ..Pool::default()
        });
        let req = request(&s, None, "claude-fable-5-1", true, &[], 1_000);
        for seed in 0..8u64 {
            assert_ne!(
                picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
                "a",
                "a spent fable window is out of the selection set under {strategy:?}"
            );
        }
    }
    let earliest = snapshot(Pool {
        strategy: Strategy::EarliestRefresh,
        ..Pool::default()
    });
    let fable = request(&earliest, None, "claude-fable-5-1", true, &[], 1_000);
    for seed in 0..8u64 {
        assert_eq!(
            picked(&select(&mut st, &earliest, &fable, &mut StdRng::seed_from_u64(seed)).0),
            "c",
            "and the rest are ordered by the fable reset, not the 7d one"
        );
    }
    let shared = request(&earliest, None, "claude-sonnet-5", true, &[], 1_000);
    for seed in 0..8u64 {
        assert_eq!(
            picked(
                &select(
                    &mut st,
                    &earliest,
                    &shared,
                    &mut StdRng::seed_from_u64(seed)
                )
                .0
            ),
            "a",
            "a request for another model neither reads the fable window nor is excluded by it"
        );
    }
}

#[test]
fn a_fable_request_with_every_window_spent_is_exhausted_at_the_earliest_fable_reset() {
    let s = snapshot(Pool {
        strategy: Strategy::EarliestRefresh,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample(
        "a",
        with_fable(refreshing(0.1, HOUR, HOUR, 1), 1.0, Some(4 * HOUR)),
    );
    st.record_sample(
        "b",
        with_fable(refreshing(0.1, HOUR, 2 * HOUR, 1), 1.0, Some(9 * HOUR)),
    );
    st.record_sample(
        "c",
        with_fable(refreshing(0.1, HOUR, 8 * HOUR, 1), 2.0, Some(3 * HOUR)),
    );
    let (result, writes) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-fable-5-1", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 3 * HOUR,
            window: "seven_day_fable".into()
        }),
        "the earliest fable reset is the wait the caller is given"
    );
    assert!(writes.is_empty(), "and nothing was pinned");
    let mut blind = RoutingState::from_load(RoutingLoad::default());
    for account in ["a", "b", "c"] {
        blind.record_sample(
            account,
            with_fable(refreshing(0.1, HOUR, HOUR, 1), 1.0, None),
        );
    }
    assert_eq!(
        select(
            &mut blind,
            &s,
            &request(&s, Some("s1"), "claude-fable-5-1", true, &[], 1_000),
            &mut rng(),
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Untimed),
        "with no reset sampled there is no deadline to hand out"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-opus-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "and every account still serves the models the fable window does not govern"
    );
}

#[test]
fn the_depletion_threshold_drops_a_depleted_account_and_falls_back_when_every_account_is_depleted()
{
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // `a` refreshes first, so it is what the refresh order would reach for, and it is the depleted one.
    st.record_sample("a", refreshing(0.95, HOUR, HOUR, 1));
    st.record_sample("b", refreshing(0.5, HOUR, 2 * HOUR, 1));
    st.record_sample("c", refreshing(0.6, HOUR, 8 * HOUR, 1));
    for strategy in [
        Strategy::EarliestRefresh,
        Strategy::LeastUtilized,
        Strategy::PriorityRandom,
    ] {
        let s = snapshot(Pool {
            strategy,
            select_below: Some(0.92),
            ..Pool::default()
        });
        let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
        for seed in 0..8u64 {
            assert_ne!(
                picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
                "a",
                "the depleted account is out of the selection set under {strategy:?}"
            );
        }
        let all = snapshot(Pool {
            strategy,
            select_below: Some(0.5),
            ..Pool::default()
        });
        let req = request(&all, None, "claude-sonnet-5", true, &[], 1_000);
        let expected = match strategy {
            Strategy::EarliestRefresh => Some("a"),
            Strategy::LeastUtilized => Some("b"),
            Strategy::PriorityRandom | Strategy::RoundRobin => None,
        };
        for seed in 0..8u64 {
            let result = select(&mut st, &all, &req, &mut StdRng::seed_from_u64(seed)).0;
            match expected {
                Some(id) => assert_eq!(
                    picked(&result),
                    id,
                    "with every account depleted the full set stands under {strategy:?}"
                ),
                None => assert!(
                    ["a", "b", "c"].contains(&picked(&result)),
                    "with every account depleted the full set stands under {strategy:?}"
                ),
            }
        }
    }
}

#[test]
fn a_pin_on_a_depleted_account_is_renewed_rather_than_moved() {
    let s = snapshot(Pool {
        strategy: Strategy::LeastUtilized,
        select_below: Some(0.8),
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", refreshing(0.1, HOUR, HOUR, 1));
    st.record_sample("b", refreshing(0.5, HOUR, HOUR, 1));
    let (opening, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(picked(&opening), "a");
    let epoch = match &opening {
        SelectResult::Selected(sel) => sel.pin.as_ref().unwrap().epoch,
        other => panic!("expected a selection, got {other:?}"),
    };
    st.record_sample("a", refreshing(0.95, HOUR, HOUR, 2));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &mut rng(),
    );
    let SelectResult::Selected(sel) = result else {
        panic!("expected a selection")
    };
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.migrated,
            sel.pin.unwrap().epoch
        ),
        ("a", None, epoch),
        "the threshold narrows a fresh selection; only the migrate rule moves a live pin"
    );
}

#[test]
fn the_migrate_rule_moves_a_hot_pin_to_a_cooler_sibling_and_keeps_it_when_none_exists() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut r
            )
            .0
        ),
        "a"
    );
    st.record_sample("a", sample(0.85, 0.3, 1_500));
    st.record_sample("b", sample(0.9, 0.3, 1_500));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
                &mut r
            )
            .0
        ),
        "a",
        "no cooler sibling, the pin stays"
    );
    st.record_sample("b", sample(0.1, 0.3, 2_500));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000),
        &mut r,
    );
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Utilization)),
        "the cooler sibling wins even at the worse priority"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 4_000),
                &mut r
            )
            .0
        ),
        "b",
        "the new pin is on b"
    );
}

#[test]
fn prefer_plan_migrates_out_of_overage_and_returns_only_when_no_sibling_is_eligible() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut r
            )
            .0
        ),
        "a"
    );
    st.record_overage("a", overage(true, 1_500, Some(9_000)));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &mut r,
    );
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Overage))
    );
    st.record_cooldown(cooled("b", "account", 2_000 + HOUR, "five_hour"));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000),
                &mut r
            )
            .0
        ),
        "a",
        "back to the overage account only when nothing else is eligible"
    );
    let never = snapshot(Pool {
        policy: OveragePolicy::Never,
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    st2.record_overage("a", overage(true, 1_500, Some(9_000)));
    assert_eq!(
        picked(
            &select(
                &mut st2,
                &never,
                &request(&never, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
                &mut r
            )
            .0
        ),
        "b",
        "never skips the overage account"
    );
    st2.record_cooldown(cooled("b", "account", 2_000 + HOUR, "five_hour"));
    let (result, _) = select(
        &mut st2,
        &never,
        &request(&never, Some("s9"), "claude-sonnet-5", true, &[], 3_000),
        &mut r,
    );
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Timed { deadline: 9_000, ref window }) if window == "overage"),
        "the shared reset is the deadline"
    );
    let allow = snapshot(Pool {
        policy: OveragePolicy::Allow,
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st3 = RoutingState::from_load(RoutingLoad::default());
    st3.record_overage("a", overage(true, 1_500, Some(9_000)));
    assert_eq!(
        picked(
            &select(
                &mut st3,
                &allow,
                &request(&allow, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
                &mut r
            )
            .0
        ),
        "a",
        "allow changes nothing"
    );
}

/// The TTL the router's default configuration pulls samples on, which is how long a reading that dates no
/// refund of its own is worth.
const SAMPLE_TTL: i64 = 60_000;

#[test]
fn a_spent_fable_window_stops_excluding_once_the_reset_it_named_passes() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", with_fable(sample(0.1, 0.1, 1), 1.0, Some(2 * HOUR)));
    assert_eq!(
        select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-fable-5-1", true, &[], HOUR),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 2 * HOUR,
            window: "seven_day_fable".into()
        }),
        "until the reset the reading stands"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s2"), "claude-fable-5-1", true, &[], 2 * HOUR),
                &mut rng()
            )
            .0
        ),
        "a",
        "a reset at or before now is the refund the sample itself announced"
    );
}

#[test]
fn a_spent_fable_window_that_dated_no_reset_stops_excluding_once_the_reading_ages_out() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", with_fable(sample(0.1, 0.1, 1_000), 1.0, None));
    assert_eq!(
        select(
            &mut st,
            &s,
            &request(
                &s,
                Some("s1"),
                "claude-fable-5-1",
                true,
                &[],
                1_000 + SAMPLE_TTL - 1
            ),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Untimed),
        "while the router would still be trusting the reading it is an exclusion"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(
                    &s,
                    Some("s2"),
                    "claude-fable-5-1",
                    true,
                    &[],
                    1_000 + SAMPLE_TTL
                ),
                &mut rng()
            )
            .0
        ),
        "a",
        "past it the account is tried again rather than excluded on a reading nothing refreshes"
    );
}

#[test]
fn a_staged_migration_keeps_the_pin_when_no_destination_holds_fable_allowance() {
    let s = snapshot(Pool {
        strategy: Strategy::LeastUtilized,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", sample(0.1, 0.1, 1));
    st.record_sample("b", sample(0.5, 0.1, 1));
    st.record_sample("c", sample(0.6, 0.1, 1));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-fable-5-1", true, &[], 1_000),
                &mut rng()
            )
            .0
        ),
        "a"
    );
    st.record_overage("a", overage(true, 1_500, Some(9_000)));
    st.record_sample("a", with_fable(sample(0.1, 0.1, 2), 0.5, Some(9 * HOUR)));
    st.record_sample("b", with_fable(sample(0.5, 0.1, 2), 1.0, Some(9 * HOUR)));
    st.record_sample("c", with_fable(sample(0.6, 0.1, 2), 1.0, Some(9 * HOUR)));
    let (result, writes) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-fable-5-1", true, &[], 2_000),
        &mut rng(),
    );
    let SelectResult::Selected(sel) = result else {
        panic!("expected the pin to be renewed, got {result:?}")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "a destination with no fable allowance left is no destination at all"
    );
    assert_eq!(
        st.pins()
            .iter()
            .find(|p| p.session_id == "s1" && p.model == "claude-fable-5-1")
            .map(|p| p.account_id.as_str()),
        Some("a"),
        "and the pin the migration would have moved is still there"
    );
    assert!(
        !writes
            .iter()
            .any(|w| matches!(w, StateWrite::DeletePin(_, _, _))),
        "so nothing deleted it on the way"
    );
    st.record_sample("b", with_fable(sample(0.5, 0.1, 3), 0.2, Some(9 * HOUR)));
    let (result, writes) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-fable-5-1", true, &[], 3_000),
        &mut rng(),
    );
    let SelectResult::Selected(sel) = result else {
        panic!("expected a migration, got {result:?}")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Overage)),
        "one sibling with allowance left is enough to move the pin"
    );
    assert!(
        writes
            .iter()
            .any(|w| matches!(w, StateWrite::DeletePin(_, _, _))),
        "and that migration releases the old pin"
    );
}

#[test]
fn a_non_portable_request_with_a_blocked_pin_gets_that_accounts_exhaustion_and_selects_normally_without_one(
) {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let first = picked(
        &select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", false, &[], 1_000),
            &mut r,
        )
        .0,
    )
    .to_string();
    st.record_cooldown(cooled(&first, "account", 1_000 + HOUR, "seven_day"));
    let (result, writes) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", false, &[], 2_000),
        &mut r,
    );
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Timed { deadline, ref window }) if deadline == 1_000 + HOUR && window == "seven_day")
    );
    assert!(writes.is_empty(), "the pin stays and nothing else is tried");
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s2"), "claude-sonnet-5", false, &[], 2_000),
        &mut r,
    );
    assert_ne!(
        picked(&result),
        first,
        "without a pin a non-portable request selects like any other"
    );
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_500),
        &mut r,
    );
    assert_ne!(
        picked(&result),
        first,
        "the same session's portable request migrates"
    );
}

#[test]
fn exhaustion_is_timed_from_the_earliest_cooldown_and_untimed_when_only_an_operator_can_clear_it() {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled("a", "account", 5_000, "five_hour"));
    st.record_cooldown(cooled("b", "model:claude-sonnet-5", 4_000, "seven_day"));
    st.record_cooldown(cooled("c", "account", 6_000, "retry_after"));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Timed { deadline: 4_000, ref window }) if window == "seven_day")
    );
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-opus-5", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(
        picked(&result),
        "b",
        "a model cooldown leaves the other models on that account eligible"
    );
    let dead = snapshot_with(
        Pool::default(),
        &[
            ("a", false, AccountState::Ready),
            ("b", true, AccountState::ReauthRequired),
            ("c", true, AccountState::FundingRequired),
        ],
    );
    let (result, _) = select(
        &mut RoutingState::from_load(RoutingLoad::default()),
        &dead,
        &request(&dead, None, "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert!(matches!(
        result,
        SelectResult::Exhausted(Exhaustion::Untimed)
    ));
    let mixed = snapshot_with(
        Pool::default(),
        &[
            ("a", true, AccountState::ReauthRequired),
            ("b", true, AccountState::FundingRequired),
        ],
    );
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    st2.record_cooldown(cooled("c", "account", 7_000, "five_hour"));
    let (result, _) = select(
        &mut st2,
        &mixed,
        &request(&mixed, None, "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert!(
        matches!(
            result,
            SelectResult::Exhausted(Exhaustion::Timed {
                deadline: 7_000,
                ..
            })
        ),
        "a mixed pool answers from the cooled account's deadline"
    );
}

#[test]
fn pin_renewal_follows_the_pool_ttl_and_affinity_off_writes_no_pin() {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 0),
        &mut r,
    );
    let SelectResult::Selected(opening) = result else {
        panic!()
    };
    let first = opening.account.id.clone();
    let epoch = opening.pin.as_ref().unwrap().epoch;
    for at in [3_500_000, 7_000_000] {
        let (result, _) = select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], at),
            &mut r,
        );
        let SelectResult::Selected(sel) = result else {
            panic!()
        };
        assert_eq!(sel.account.id, first);
        assert_eq!(
            sel.pin.as_ref().unwrap().epoch,
            epoch,
            "a renewal keeps the pin, so its epoch does not move"
        );
        assert_eq!(st.pins()[0].created_at, 0, "and neither does created_at");
        assert_eq!(st.pins()[0].last_used_at, at, "only last_used_at does");
    }
    let (_, writes) = select(
        &mut st,
        &s,
        &request(
            &s,
            Some("s1"),
            "claude-sonnet-5",
            true,
            &[],
            7_000_000 + 3_601_000,
        ),
        &mut r,
    );
    assert!(
        writes.iter().any(
            |w| matches!(w, StateWrite::UpsertPin(p) if p.created_at == 7_000_000 + 3_601_000)
        ),
        "a 3,601 s gap expires the pin and a fresh one is written"
    );
    let short = snapshot(Pool {
        affinity: 600,
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st2,
        &short,
        &request(&short, Some("s1"), "claude-sonnet-5", true, &[], 0),
        &mut r,
    );
    let (_, writes) = select(
        &mut st2,
        &short,
        &request(&short, Some("s1"), "claude-sonnet-5", true, &[], 601_000),
        &mut r,
    );
    assert!(writes
        .iter()
        .any(|w| matches!(w, StateWrite::UpsertPin(p) if p.created_at == 601_000)));
    let off = snapshot(Pool {
        affinity: 0,
        ..Pool::default()
    });
    let mut st3 = RoutingState::from_load(RoutingLoad::default());
    let mut seen = std::collections::HashSet::new();
    for i in 0..40u64 {
        let (result, writes) = select(
            &mut st3,
            &off,
            &request(&off, Some("s1"), "claude-sonnet-5", true, &[], i as i64),
            &mut StdRng::seed_from_u64(i),
        );
        assert!(writes.is_empty(), "affinity off writes nothing");
        seen.insert(picked(&result).to_string());
    }
    assert_eq!(seen.len(), 3);
}

#[test]
fn overage_evidence_only_moves_forward_and_a_pin_is_invalidated_only_by_its_own_epoch() {
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert!(st.record_overage("a", overage(true, 100, Some(900))));
    assert!(
        !st.record_overage("a", overage(false, 90, None)),
        "a late head cannot clear it"
    );
    assert!(st.in_overage("a"));
    assert!(st.record_overage("a", overage(false, 110, None)));
    assert!(!st.in_overage("a"));
    let s = snapshot(Pool::default());
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    let pin = sel.pin.unwrap();
    let other = PinRef {
        epoch: pin.epoch + 1,
        ..pin.clone()
    };
    assert!(
        !st.invalidate_pin(&other, &sel.account.id),
        "a newer epoch is another pin"
    );
    assert!(
        !st.invalidate_pin(&pin, "zzz"),
        "another account's failure leaves it alone"
    );
    assert!(st.invalidate_pin(&pin, &sel.account.id));
    assert!(st.pins().is_empty());
}

#[test]
fn a_non_portable_pin_survives_member_removal_and_model_deselection_and_answers_that_accounts_exhaustion(
) {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let first = picked(
        &select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", false, &[], 1_000),
            &mut r,
        )
        .0,
    )
    .to_string();
    // The pinned account is no longer a candidate: the caller's candidates come from a snapshot without it.
    let without: Vec<phoebis_routing::Candidate> = resolve(&s, s.key("k").unwrap(), "claude-sonnet-5")
        .unwrap()
        .into_iter()
        .filter(|c| c.account_id != first)
        .collect();
    let req = SelectRequest {
        candidates: without.clone(),
        ..request(&s, Some("s1"), "claude-sonnet-5", false, &[], 2_000)
    };
    let (result, writes) = select(&mut st, &s, &req, &mut r);
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "the removed member's exhaustion, not another account"
    );
    assert!(writes.is_empty(), "the pin is kept for the fence");
    assert_eq!(st.pins()[0].account_id, first);
    st.record_cooldown(cooled(&first, "model:claude-sonnet-5", 9_000, "seven_day"));
    let (result, _) = select(&mut st, &s, &req, &mut r);
    assert!(
        matches!(
            result,
            SelectResult::Exhausted(Exhaustion::Timed {
                deadline: 9_000,
                ..
            })
        ),
        "a cooled removed member answers timed"
    );
    let portable = SelectRequest {
        candidates: without,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000)
    };
    let (result, writes) = select(&mut st, &s, &portable, &mut r);
    assert_ne!(
        picked(&result),
        first,
        "the same session's portable request moves on"
    );
    assert!(
        writes
            .iter()
            .any(|w| matches!(w, StateWrite::DeletePin(..))),
        "and only then is the old pin dropped"
    );
}

#[test]
fn a_candidate_the_snapshot_no_longer_holds_is_never_eligible() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    req.candidates.push(phoebis_routing::Candidate {
        account_id: "deleted".into(),
        priority: 0,
        upstream_model: "claude-sonnet-5".into(),
    });
    let mut r = rng();
    for i in 0..10 {
        req.now = i;
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut r).0),
            "a",
            "a deleted priority-zero account never wins"
        );
    }
    req.attempted = vec!["a".into(), "deleted".into()];
    assert_eq!(picked(&select(&mut st, &s, &req, &mut rng()).0), "b");
    req.attempted = vec!["a".into(), "b".into(), "deleted".into()];
    assert!(matches!(
        select(&mut st, &s, &req, &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::Untimed)
    ));
}

#[test]
fn a_paused_account_serves_no_request() {
    let s = snapshot_with(
        Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        },
        &[("a", false, AccountState::Ready)],
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    for i in 0..10 {
        assert_eq!(
            picked(
                &select(
                    &mut st,
                    &s,
                    &request(&s, None, "claude-sonnet-5", true, &[], i),
                    &mut rng()
                )
                .0
            ),
            "b"
        );
    }
}

#[test]
fn an_attempted_account_still_dates_the_exhaustion_with_its_cooldown() {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled("a", "account", 8_000, "five_hour"));
    st.record_cooldown(cooled("b", "account", 5_000, "seven_day"));
    st.record_cooldown(cooled("c", "account", 6_000, "retry_after"));
    let all = &["a", "b", "c"];
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-5", true, all, 1_000),
        &mut rng(),
    );
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Timed { deadline: 5_000, ref window }) if window == "seven_day"),
        "three accounts rate-limited inside one request answer with the earliest reset"
    );
    st.clear_cooldown("b", "account");
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-5", true, all, 1_000),
        &mut rng(),
    );
    assert!(
        matches!(
            result,
            SelectResult::Exhausted(Exhaustion::Timed {
                deadline: 6_000,
                ..
            })
        ),
        "one attempted account without a cooldown does not hide the others' deadlines"
    );
    st.clear_cooldown("a", "account");
    st.clear_cooldown("c", "account");
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-5", true, all, 1_000),
        &mut rng(),
    );
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Untimed)),
        "an attempt on its own is not a deadline anyone can wait for"
    );
}

#[test]
fn a_tier_of_nan_samples_still_selects() {
    let s = snapshot(Pool {
        strategy: Strategy::LeastUtilized,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    for account in ["a", "b", "c"] {
        st.record_sample(account, sample(f64::NAN, f64::NAN, 1));
    }
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert!(["a", "b", "c"].contains(&picked(&result)));
}

#[test]
fn the_first_overage_evidence_normalizes_since_like_every_later_one() {
    let mut clear = RoutingState::from_load(RoutingLoad::default());
    clear.record_overage("a", overage(false, 100, None));
    assert_eq!(
        clear.overage("a").unwrap().since,
        None,
        "an account out of overage has no start"
    );
    let mut entering = RoutingState::from_load(RoutingLoad::default());
    entering.record_overage(
        "a",
        OverageEvidence {
            in_use: true,
            since: None,
            reset: None,
            observed_at: 100,
            source: EvidenceSource::Sample,
        },
    );
    assert_eq!(
        entering.overage("a").unwrap().since,
        Some(100),
        "evidence with no start of its own takes the observation time"
    );
}
