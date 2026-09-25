use phoebis_routing::{
    release_reasons, resolve, select, EvidenceSource, Exhaustion, MigrateReason, OverageEvidence,
    PinRef, RegistrySnapshot, RoutingState, SelectRequest, SelectResult, StateWrite,
    UtilizationSample, WindowReading, FIVE_HOUR, MODEL_REJECTED_REASON, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, Failover, OveragePolicy, PinRow,
    PoolMember, PoolRow, RegistryLoad, RoutingLoad, RulesMode, Shedding, Strategy,
    SubscriptionTerm,
};
use rand::rngs::StdRng;
use rand::SeedableRng;

const HOUR: i64 = 3_600_000;
/// `RoutingState::from_load` seeds this from `DEFAULT_SAMPLE_TTL_MS`, private to that crate; duplicated here
/// since a state built with no override reads that default.
const SAMPLE_TTL_MS: i64 = 60_000;
/// The fallback `request()` carries, for a call to `in_overage` outside a full `SelectRequest`.
const FALLBACK_MS: i64 = 60_000;

struct Pool {
    policy: OveragePolicy,
    strategy: Strategy,
    affinity: u64,
    migrate: Option<f64>,
    select_below: Option<f64>,
    members: Vec<(&'static str, u32)>,
    prefer_subscription: bool,
    kinds: Vec<(&'static str, AccountKind)>,
    failover: Failover,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            policy: OveragePolicy::PreferPlan,
            strategy: Strategy::Priority,
            affinity: 3600,
            migrate: None,
            select_below: None,
            members: vec![("a", 0), ("b", 0), ("c", 0)],
            prefer_subscription: false,
            kinds: vec![],
            failover: Failover::default(),
        }
    }
}

fn snapshot(pool: Pool) -> RegistrySnapshot {
    snapshot_with(pool, &[])
}

fn snapshot_with(pool: Pool, unavailable: &[(&str, bool, AccountState)]) -> RegistrySnapshot {
    RegistrySnapshot::from_load(load_of(pool, unavailable), [0; 32], 0, 1)
}

/// As `snapshot_with`, but with a `SubscriptionTerm` per account, for a test over the end, the wind-down, or
/// the cancelled obstacle.
fn snapshot_with_terms(pool: Pool, terms: Vec<SubscriptionTerm>) -> RegistrySnapshot {
    let mut load = load_of(pool, &[]);
    load.terms = terms;
    RegistrySnapshot::from_load(load, [0; 32], 0, 1)
}

/// `unavailable` names accounts inserted disabled or in a state the eligible set excludes.
fn load_of(pool: Pool, unavailable: &[(&str, bool, AccountState)]) -> RegistryLoad {
    let account = |id: &str| {
        let (enabled, state) = unavailable
            .iter()
            .find(|(u, _, _)| *u == id)
            .map(|(_, e, s)| (*e, *s))
            .unwrap_or((true, AccountState::Ready));
        let kind = pool
            .kinds
            .iter()
            .find(|(a, _)| *a == id)
            .map(|(_, k)| *k)
            .unwrap_or(AccountKind::Oauth);
        AccountRow {
            id: id.into(),
            kind,
            label: id.into(),
            enabled,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state,
        }
    };
    RegistryLoad {
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
            model_rules: Vec::new(),
            model_rules_mode: RulesMode::Extend,
            long_context_twins: true,
            keepalive_opt_out: false,
            body_capture_hours: None,
            secret_digest: vec![0; 32],
            prefix: "00000000".into(),
            created_at: 0,
            rotated_at: None,
            last_used_at: None,
            retired_secret_last_used_at: None,
            in_flight_cap: None,
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
        pools: vec![PoolRow {
            id: "p".into(),
            label: "p".into(),
            enabled: true,
            strategy: pool.strategy,
            affinity_ttl_seconds: pool.affinity,
            overage_policy: pool.policy,
            migrate_above_utilization: pool.migrate,
            select_below_utilization: pool.select_below,
            model_rules: Vec::new(),
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
            prefer_subscription: pool.prefer_subscription,
            shedding: Shedding::default(),
            failover: pool.failover,
            cache_ttl_json: "{}".into(),
            cache_diagnostics: None,
            created_at: 0,
            updated_at: 0,
        }],
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
        terms: Vec::new(),
    }
}

/// `s` rebuilt with `id` entirely missing from `accounts` and from every pool's membership, the way a deleted
/// account leaves the registry.
fn without_account(s: &RegistrySnapshot, id: &str) -> RegistrySnapshot {
    let load = RegistryLoad {
        accounts: s.accounts().filter(|a| a.id != id).cloned().collect(),
        credential_generations: Vec::new(),
        keys: s.keys().cloned().collect(),
        members: s
            .pools()
            .flat_map(|p| s.members(&p.id).to_vec())
            .filter(|m| m.account_id != id)
            .collect(),
        pools: s.pools().cloned().collect(),
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
        terms: Vec::new(),
    };
    RegistrySnapshot::from_load(load, [0; 32], 0, 1)
}

/// `s` rebuilt with key "k" carrying a different `created_at`, the generation a delete and a recreate under
/// the same id leaves; every other row reads exactly as `s`'s did.
fn key_recreated(s: &RegistrySnapshot, created_at: i64) -> RegistrySnapshot {
    let load = RegistryLoad {
        accounts: s.accounts().cloned().collect(),
        credential_generations: Vec::new(),
        keys: s
            .keys()
            .cloned()
            .map(|mut k| {
                if k.id == "k" {
                    k.created_at = created_at;
                }
                k
            })
            .collect(),
        members: s.pools().flat_map(|p| s.members(&p.id).to_vec()).collect(),
        pools: s.pools().cloned().collect(),
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
        terms: Vec::new(),
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
        key_generation: s.key("k").unwrap().created_at,
        session_id: session.map(String::from),
        canonical_model: model.into(),
        candidates: resolve(s, s.key("k").unwrap(), model, model).unwrap(),
        portable,
        attempted: attempted.iter().map(|a| a.to_string()).collect(),
        now,
        fallback_ms: 60_000,
        revision: s.revision,
    }
}

fn cooled(account: &str, scope: &str, until: i64, window: &str, reason: &str) -> CooldownRow {
    CooldownRow {
        account_id: account.into(),
        scope: scope.into(),
        until,
        set_at: 0,
        reason: reason.into(),
        window: window.into(),
        source_request_id: None,
        incarnation: 1,
    }
}

fn overage(in_use: bool, observed_at: i64, reset: Option<i64>) -> OverageEvidence {
    OverageEvidence {
        in_use,
        since: Some(observed_at),
        reset,
        observed_at,
        source: EvidenceSource::Head,
        incarnation: 1,
    }
}

fn sample(five_hour: f64, seven_day: f64, observed_at: i64) -> UtilizationSample {
    UtilizationSample {
        observed_at,
        ..Default::default()
    }
    .with(
        FIVE_HOUR,
        WindowReading {
            utilization: five_hour,
            reset: None,
            observed_at,
        },
    )
    .with(
        SEVEN_DAY,
        WindowReading {
            utilization: seven_day,
            reset: None,
            observed_at,
        },
    )
}

/// The window resets the refresh order reads, over the utilization `sample` already carries.
fn refreshing(
    five_hour: f64,
    five_hour_reset: i64,
    seven_day_reset: i64,
    observed_at: i64,
) -> UtilizationSample {
    sample(five_hour, 0.1, observed_at)
        .with(
            FIVE_HOUR,
            WindowReading {
                utilization: five_hour,
                reset: Some(five_hour_reset),
                observed_at,
            },
        )
        .with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.1,
                reset: Some(seven_day_reset),
                observed_at,
            },
        )
}

/// The fable allowance and its reset over a sample that already carries the shared windows.
fn with_fable(base: UtilizationSample, fable: f64, fable_reset: Option<i64>) -> UtilizationSample {
    let observed_at = base.observed_at;
    base.with(
        SEVEN_DAY_FABLE,
        WindowReading {
            utilization: fable,
            reset: fable_reset,
            observed_at,
        },
    )
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
    st.record_cooldown(cooled(
        "a",
        "account",
        1_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
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
fn a_fresh_pin_carries_the_generation_the_request_was_admitted_under_not_the_snapshots() {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    // Admitted against the key's original generation; the snapshot select reads from names a later one, as
    // a delete and a recreate of the same id between admission and this call would leave it.
    let req = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000);
    let recreated = key_recreated(&s, 500);
    let (_, writes) = select(&mut st, &recreated, &req, &mut r);
    assert!(
        writes.iter().any(|w| matches!(
            w,
            StateWrite::UpsertPin(p) if p.key_generation == req.key_generation
        )),
        "the pin is stamped with the generation the relay admitted the request under"
    );
    assert!(
        !writes
            .iter()
            .any(|w| matches!(w, StateWrite::UpsertPin(p) if p.key_generation == 500)),
        "not with the generation the id happens to carry in the snapshot select reads from"
    );
}

#[test]
fn pace_ranks_remaining_weekly_allowance_over_hours_to_reset_and_scores_an_unsampled_account_as_untouched(
) {
    let s = snapshot(Pool {
        strategy: Strategy::Pace,
        ..Pool::default()
    });
    // `window_for` reads the shared plan for a non-fable model, so only `seven_day` and its reset matter here.
    let reading = |seven_day: f64, reset_at: i64, observed_at: i64| {
        UtilizationSample {
            observed_at,
            ..Default::default()
        }
        .with(
            SEVEN_DAY,
            WindowReading {
                utilization: seven_day,
                reset: Some(reset_at),
                observed_at,
            },
        )
    };
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", reading(0.5, 1_000 + 24 * HOUR, 1));
    st.record_sample("b", reading(0.2, 1_000 + 96 * HOUR, 1));
    // `c` stays unsampled: untouched, remaining 1 over the full week.
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    for seed in 0..8u64 {
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
            "a",
            "0.5 used resetting in 24h (0.0208) outranks 0.2 used resetting in 96h (0.0083) and an untouched account (1/168)"
        );
    }
    st.record_sample("a", reading(0.9, 1_000 + 24 * HOUR, 2));
    for seed in 0..8u64 {
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
            "b",
            "spent to 0.1 remaining (0.0042), a falls behind b's wider allowance"
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
        "with nobody sampled every account scores the same untouched reading and the tie is random"
    );
    let mut two_tied = RoutingState::from_load(RoutingLoad::default());
    // Far below the untouched score, so `b` never wins the draw between the two accounts that tie for it.
    two_tied.record_sample("b", reading(0.99, 1_000 + 168 * HOUR, 1));
    let mut seen = std::collections::HashSet::new();
    for i in 0..40u64 {
        let (result, _) = select(
            &mut two_tied,
            &s,
            &request(&s, None, "claude-sonnet-5", true, &[], 1_000),
            &mut StdRng::seed_from_u64(i),
        );
        seen.insert(picked(&result).to_string());
    }
    assert_eq!(
        seen,
        std::collections::HashSet::from(["a".to_string(), "c".to_string()]),
        "a and c tie untouched, and both are seen over the seeded draw"
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
    for strategy in [Strategy::Pace, Strategy::Priority, Strategy::RoundRobin] {
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
        strategy: Strategy::Pace,
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
        strategy: Strategy::Pace,
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
    for strategy in [Strategy::Pace, Strategy::Priority, Strategy::RoundRobin] {
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
            Strategy::Pace => Some("a"),
            Strategy::Priority | Strategy::RoundRobin => None,
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
        strategy: Strategy::Pace,
        select_below: Some(0.8),
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // No reset on either reading, so Pace falls back to the full week for both and ranks on remaining alone.
    st.record_sample("a", sample(0.1, 0.1, 1));
    st.record_sample("b", sample(0.5, 0.5, 1));
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
    st.record_cooldown(cooled(
        "b",
        "account",
        2_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
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
    st2.record_cooldown(cooled(
        "b",
        "account",
        2_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
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
        strategy: Strategy::Pace,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // The opening pick is a fable request, so it is the fable reading Pace ranks by; no reset on any of the
    // three, so Pace falls back to the full week for all and ranks on remaining alone.
    st.record_sample("a", with_fable(sample(0.1, 0.1, 1), 0.1, None));
    st.record_sample("b", with_fable(sample(0.5, 0.3, 1), 0.3, None));
    st.record_sample("c", with_fable(sample(0.6, 0.4, 1), 0.4, None));
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
    st.record_cooldown(cooled(
        &first,
        "account",
        1_000 + HOUR,
        "seven_day",
        "rate_limit",
    ));
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
    st.record_cooldown(cooled("a", "account", 5_000, "five_hour", "rate_limit"));
    st.record_cooldown(cooled(
        "b",
        "model:claude-sonnet-5",
        4_000,
        "seven_day",
        "rate_limit",
    ));
    st.record_cooldown(cooled("c", "account", 6_000, "retry_after", "rate_limit"));
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
    st2.record_cooldown(cooled("c", "account", 7_000, "five_hour", "rate_limit"));
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
    assert!(st.in_overage("a", 100, FALLBACK_MS));
    assert!(st.record_overage("a", overage(false, 110, None)));
    assert!(!st.in_overage("a", 110, FALLBACK_MS));
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
    let without: Vec<phoebis_routing::Candidate> = resolve(
        &s,
        s.key("k").unwrap(),
        "claude-sonnet-5",
        "claude-sonnet-5",
    )
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
    st.record_cooldown(cooled(
        &first,
        "model:claude-sonnet-5",
        9_000,
        "seven_day",
        "rate_limit",
    ));
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
        incarnation: 1,
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
    st.record_cooldown(cooled("a", "account", 8_000, "five_hour", "rate_limit"));
    st.record_cooldown(cooled("b", "account", 5_000, "seven_day", "rate_limit"));
    st.record_cooldown(cooled("c", "account", 6_000, "retry_after", "rate_limit"));
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
        strategy: Strategy::Pace,
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
            incarnation: 1,
        },
    );
    assert_eq!(
        entering.overage("a").unwrap().since,
        Some(100),
        "evidence with no start of its own takes the observation time"
    );
}

#[test]
fn round_robin_serves_the_first_tier_in_turn_and_a_tier_change_starts_its_own_round() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0), ("c", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let pick = |st: &mut RoutingState, r: &mut StdRng, i: i64| {
        picked(
            &select(
                st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], i),
                r,
            )
            .0,
        )
        .to_string()
    };
    assert_eq!(
        [
            pick(&mut st, &mut r, 1),
            pick(&mut st, &mut r, 2),
            pick(&mut st, &mut r, 3)
        ],
        ["a", "b", "a"]
    );
    st.record_cooldown(cooled(
        "a",
        "account",
        1_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    st.record_cooldown(cooled(
        "b",
        "account",
        1_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    assert_eq!(
        pick(&mut st, &mut r, 4),
        "c",
        "the pointer of tier 0 does not reach tier 1"
    );
    st.clear_cooldown("a", "account");
    st.clear_cooldown("b", "account");
    assert_eq!(
        pick(&mut st, &mut r, 5),
        "b",
        "tier 0 resumes where its own pointer left off"
    );
}

#[test]
fn a_singleton_models_round_does_not_move_a_three_account_models_round() {
    // A live cooldown narrows one model's eligible set to a single account while the other model's stays at
    // three, so one pool and one tier hold two cohorts. A cursor shared by both would be reset to 1 by every
    // opus request and `c` would never serve a sonnet one.
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0), ("c", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled(
        "b",
        "model:claude-opus-5",
        100 * HOUR,
        "seven_day",
        "rate_limit",
    ));
    st.record_cooldown(cooled(
        "c",
        "model:claude-opus-5",
        100 * HOUR,
        "seven_day",
        "rate_limit",
    ));
    let mut r = rng();
    let pick = |st: &mut RoutingState, r: &mut StdRng, model: &str, i: i64| {
        picked(&select(st, &s, &request(&s, None, model, true, &[], i), r).0).to_string()
    };
    let mut served = std::collections::HashSet::new();
    for i in 0..6 {
        assert_eq!(
            pick(&mut st, &mut r, "claude-opus-5", 2 * i + 1),
            "a",
            "the singleton model has one candidate"
        );
        served.insert(pick(&mut st, &mut r, "claude-sonnet-5", 2 * i + 2));
    }
    assert_eq!(
        served.len(),
        3,
        "every eligible account of the three-account model serves inside six interleaved rounds: {served:?}"
    );
}

#[test]
fn prefer_subscription_orders_plan_oauth_then_overage_oauth_then_api_keys_across_priorities() {
    let s = snapshot(Pool {
        strategy: Strategy::Priority,
        prefer_subscription: true,
        members: vec![("a", 1), ("b", 0), ("c", 0)],
        kinds: vec![("c", AccountKind::ApiKey)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    // `b` is oauth in overage under prefer_plan, `c` an api_key at priority 0, `a` oauth with plan quota at priority 1.
    st.record_overage("b", overage(true, 1, None));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], 1_000),
                &mut r
            )
            .0
        ),
        "a",
        "plan quota wins across the priority tiers"
    );
    st.record_cooldown(cooled(
        "a",
        "account",
        1_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], 2_000),
                &mut r
            )
            .0
        ),
        "b",
        "overage oauth before any api_key"
    );
    st.record_cooldown(cooled(
        "b",
        "account",
        1_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], 3_000),
                &mut r
            )
            .0
        ),
        "c",
        "the api_key group serves last"
    );
}

#[test]
fn an_explicitly_chosen_strategy_orders_the_api_key_group_itself() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        prefer_subscription: true,
        kinds: vec![
            ("a", AccountKind::ApiKey),
            ("b", AccountKind::ApiKey),
            ("c", AccountKind::ApiKey),
        ],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let three: Vec<String> = (1..=3)
        .map(|i| {
            picked(
                &select(
                    &mut st,
                    &s,
                    &request(&s, None, "claude-sonnet-5", true, &[], i * 1_000),
                    &mut r,
                )
                .0,
            )
            .to_string()
        })
        .collect();
    assert_eq!(
        three,
        ["a", "b", "c"],
        "the pool's own round robin serves the api_key tier in turn"
    );
}

#[test]
fn the_migrate_rule_moves_a_hot_pin_only_to_a_destination_under_the_band() {
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
    st.record_sample("b", sample(0.77, 0.3, 1_500));
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
        "0.77 is inside the band under 0.8, so b is no destination"
    );
    st.record_sample("b", sample(0.70, 0.3, 2_500));
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
        ("b", Some(MigrateReason::Utilization))
    );
    // A fresh pin isolates the two inequalities the band applies to each window: `a` sits at
    // exactly the threshold throughout, so every call below also proves the pinned side of `>=` is inclusive.
    let mut edges = RoutingState::from_load(RoutingLoad::default());
    let mut r2 = rng();
    assert_eq!(
        picked(
            &select(
                &mut edges,
                &s,
                &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 10_000),
                &mut r2
            )
            .0
        ),
        "a"
    );
    edges.record_sample("a", sample(0.8, 0.1, 10_500));
    edges.record_sample("b", sample(0.1, 0.76, 10_500));
    assert_eq!(
        picked(
            &select(
                &mut edges,
                &s,
                &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 11_000),
                &mut r2
            )
            .0
        ),
        "a",
        "b's seven-day sample sits inside the band, so the seven-day conjunct alone keeps it out"
    );
    edges.record_sample("b", sample(0.75, 0.1, 11_500));
    assert_eq!(
        picked(
            &select(
                &mut edges,
                &s,
                &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 12_000),
                &mut r2
            )
            .0
        ),
        "a",
        "exactly threshold minus the band is still inside it: the comparison is strict"
    );
    edges.record_sample("b", sample(0.1, 0.1, 12_500));
    let (result, _) = select(
        &mut edges,
        &s,
        &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 13_000),
        &mut r2,
    );
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Utilization)),
        "clearly under the band, b finally qualifies"
    );
}

#[test]
fn a_pin_on_a_spent_five_hour_window_moves_to_a_sibling_with_allowance_and_stays_without_one() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    st.record_sample("a", refreshing(0.97, 9 * HOUR, 9 * HOUR, 1_500));
    let SelectResult::Selected(sel) = pick(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "a fresh sample under 1.0 is allowance, whatever the threshold-free pool thinks of it"
    );
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 2_500));
    st.record_sample("b", refreshing(1.0, 9 * HOUR, 9 * HOUR, 2_500));
    let SelectResult::Selected(sel) = pick(&mut st, 3_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "spent with no sibling holding allowance: the pin stays and the upstream's answer decides"
    );
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 3_500));
    let (result, writes) = pick(&mut st, 4_000);
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent))
    );
    assert!(
        writes
            .iter()
            .any(|w| matches!(w, StateWrite::UpsertPin(p) if p.account_id == "b")),
        "a spent migration writes a fresh pin onto the destination: {writes:?}"
    );
    // The reset branch: s1's pin already moved to b above, so a fresh session exercises the reset check on
    // the current account itself rather than on a sibling. Both picks share one `now` so the pin's own TTL
    // never enters into it.
    let pick2 = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s2"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    let now = 9 * HOUR + 1_000;
    st.record_sample("a", refreshing(0.3, 9 * HOUR, 9 * HOUR, 4_000));
    assert_eq!(picked(&pick2(&mut st, now).0), "a");
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 4_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 4_500));
    let SelectResult::Selected(sel) = pick2(&mut st, now).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "a's reset has already passed, so the reading no longer stands and a holds allowance again"
    );
    // The no-reset branch: absent a reset, a full reading stands only for the sample TTL. Continuing s2's pin
    // on a with a fresh no-reset spent reading exercises the "still fresh" half: the account cannot serve, so
    // the pin moves.
    let t = now + 1_000;
    st.record_sample("a", sample(1.0, 0.1, t));
    st.record_sample("b", refreshing(0.2, 9 * HOUR, 9 * HOUR, t));
    let SelectResult::Selected(sel) = pick2(&mut st, t + 1_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "no reset dated: a full reading still stands for the sample TTL"
    );
    // The same no-reset reading, on a fresh pin, once the sample TTL has passed: it no longer stands.
    let pick3 = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s3"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    let t2 = t + 1_000;
    assert_eq!(picked(&pick3(&mut st, t2).0), "a");
    st.record_sample("a", sample(1.0, 0.1, t2));
    let SelectResult::Selected(sel) = pick3(&mut st, t2 + SAMPLE_TTL_MS + 1).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "no reset dated: the reading ages out once the sample TTL passes, and a holds allowance again"
    );
}

#[test]
fn a_spent_fable_window_moves_a_fable_pin_and_leaves_a_sonnet_pin_on_the_same_account_alone() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let fable = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-fable-5-1", true, &[], now),
            &mut rng(),
        )
    };
    let sonnet = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&fable(&mut st, 1_000).0), "a");
    assert_eq!(picked(&sonnet(&mut st, 1_000).0), "a");
    st.record_sample(
        "a",
        with_fable(sample(0.2, 0.1, 1_500), 1.0, Some(9 * HOUR)),
    );
    st.record_sample(
        "b",
        with_fable(sample(0.3, 0.1, 1_500), 0.5, Some(9 * HOUR)),
    );
    let SelectResult::Selected(sel) = fable(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "the fable window is spent on a and b holds fable allowance"
    );
    let SelectResult::Selected(sel) = sonnet(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "the sonnet pin reads the shared plan, neither of whose readings is spent"
    );
}

#[test]
fn a_spent_window_moves_a_pin_still_inside_the_utilization_dwell() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    st.record_sample("a", refreshing(0.85, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.3, 9 * HOUR, 9 * HOUR, 1_500));
    let SelectResult::Selected(sel) = pick(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Utilization))
    );
    // Inside the five-minute dwell, b's window is spent and a has recovered: the hover rule would hold the pin,
    // the spent rule moves it.
    st.record_sample("b", refreshing(1.0, 9 * HOUR, 9 * HOUR, 2_500));
    st.record_sample("a", refreshing(0.2, 9 * HOUR, 9 * HOUR, 2_500));
    let (result, writes) = pick(&mut st, 3_000);
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", Some(MigrateReason::Spent))
    );
    assert!(writes
        .iter()
        .any(|w| matches!(w, StateWrite::DeletePin(_, _, _))));
}

#[test]
fn a_spent_pin_moves_to_a_sibling_the_band_would_refuse() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    // b reads over the threshold, so the band refuses it as a utilization destination, and under 1.0, so it
    // holds allowance.
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.9, 9 * HOUR, 9 * HOUR, 1_500));
    let (result, writes) = pick(&mut st, 2_000);
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "the band would keep the pin on a; the spent window moves it"
    );
    assert!(
        writes
            .iter()
            .any(|w| matches!(w, StateWrite::UpsertPin(p) if p.account_id == "b")),
        "a spent migration writes a fresh pin onto the destination: {writes:?}"
    );
}

#[test]
fn a_spent_pin_stays_when_its_only_sibling_is_cooled() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    // b holds allowance and is the only sibling, but its cooldown takes it out of the eligible set before
    // the spent block looks for a destination.
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_cooldown(cooled(
        "b",
        "account",
        2_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    let (result, writes) = pick(&mut st, 2_000);
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "with nowhere to move, the pin stays and the upstream's answer decides"
    );
    assert!(
        !writes
            .iter()
            .any(|w| matches!(w, StateWrite::UpsertPin(p) if p.account_id != "a")),
        "the pin was moved off a: {writes:?}"
    );
}

#[test]
fn a_pin_on_a_cooled_account_whose_window_is_spent_still_moves_with_reason_spent() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    st.record_cooldown(cooled(
        "a",
        "account",
        2_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
    let SelectResult::Selected(sel) = pick(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "the spent move stands whatever else the selection found against the pinned account"
    );
}

#[test]
fn the_release_reasons_name_every_fact_against_the_pinned_account_in_the_selections_order() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let req = |now: i64, attempted: &[&str]| {
        request(&s, Some("s1"), "claude-sonnet-5", true, attempted, now)
    };
    assert_eq!(
        picked(&select(&mut st, &s, &req(1_000, &[]), &mut rng()).0),
        "a"
    );
    let pin = st.pin("k", "s1", "claude-sonnet-5").cloned().unwrap();
    assert_eq!(
        release_reasons(&st, &s, &pin, &req(2_000, &[])),
        vec!["released"],
        "nothing stands against a live pin on a serving account"
    );
    st.record_cooldown(cooled(
        "a",
        "account",
        2_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    assert_eq!(
        release_reasons(&st, &s, &pin, &req(2_000, &[])),
        vec!["cooldown", "spent"]
    );
    assert_eq!(
        release_reasons(&st, &s, &pin, &req(2_000, &["a"])),
        vec!["cooldown", "attempted", "spent"]
    );
    assert_eq!(
        release_reasons(&st, &s, &pin, &req(1_000 + 3_600_001, &[])),
        vec!["expired", "cooldown", "spent"],
        "the idle pin is expired first, and the facts behind it still follow"
    );
    let paused = snapshot_with(
        Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        },
        &[("a", false, AccountState::ReauthRequired)],
    );
    let quiet = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        release_reasons(&quiet, &paused, &pin, &request(&paused, Some("s1"), "claude-sonnet-5", true, &[], 2_000)),
        vec!["paused", "reauth_required"],
        "a disabled account in a state that cannot serve names both facts, still resolved as a candidate"
    );
    let gone = without_account(&s, "a");
    assert_eq!(
        release_reasons(
            &quiet,
            &gone,
            &pin,
            &request(&gone, Some("s1"), "claude-sonnet-5", true, &[], 2_000)
        ),
        vec!["unserved", "deleted"]
    );
}

#[test]
fn a_pin_both_in_overage_and_spent_records_overage_and_lands_where_both_allow() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1), ("c", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    // a is in overage and spent; b is out of overage with allowance; c is out of overage and spent.
    st.record_overage("a", overage(true, 1_500, Some(9_000)));
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("c", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    let SelectResult::Selected(sel) = pick(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Overage)),
        "overage is the recorded reason, and b is the one sibling both predicates allow"
    );
}

#[test]
fn a_spent_pin_under_prefer_plan_stays_rather_than_moving_onto_a_sibling_in_overage() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    // a's five-hour window stands full while b is in known overage with no sample of its own, which is what
    // a restart leaves: the overage rows are reloaded and the samples start empty, so b reads as holding
    // allowance. Moving the pin there would hand it straight back on the next request.
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_overage("b", overage(true, 1_500, Some(9 * HOUR)));
    for now in [2_000, 3_000] {
        let SelectResult::Selected(sel) = pick(&mut st, now).0 else {
            panic!()
        };
        assert_eq!(
            (sel.account.id.as_str(), sel.migrated),
            ("a", None),
            "the session stays and the upstream's answer decides, as with no sibling at all"
        );
    }
}

#[test]
fn a_spent_pin_under_allow_moves_onto_a_sibling_in_overage_once_and_is_not_moved_back() {
    let s = snapshot(Pool {
        policy: OveragePolicy::Allow,
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_overage("b", overage(true, 1_500, Some(9 * HOUR)));
    let SelectResult::Selected(sel) = pick(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "overage is what this pool pays for, so the spent pin takes the sibling holding allowance"
    );
    let SelectResult::Selected(sel) = pick(&mut st, 3_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", None),
        "and nothing moves it back"
    );
}

#[test]
fn a_spent_seven_day_window_moves_the_pin_as_a_spent_five_hour_one_does() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, 1_000).0), "a");
    // a's five-hour reading is well under 1.0; its seven-day reading is the spent one, with its reset ahead.
    st.record_sample(
        "a",
        sample(0.3, 1.0, 1_500).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 1.0,
                reset: Some(9 * HOUR),
                observed_at: 1_500,
            },
        ),
    );
    st.record_sample("b", sample(0.2, 0.2, 1_500));
    let (result, writes) = pick(&mut st, 2_000);
    let SelectResult::Selected(sel) = result else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "the seven-day reading at 1.0 spends the shared plan as the five-hour one does"
    );
    assert!(
        writes
            .iter()
            .any(|w| matches!(w, StateWrite::UpsertPin(p) if p.account_id == "b")),
        "a spent migration writes a fresh pin onto the destination: {writes:?}"
    );
}

#[test]
fn purge_drops_a_round_robin_cursor_once_its_cohort_account_is_gone() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], 1_000),
                &mut r
            )
            .0
        ),
        "a",
        "the cursor advances past a"
    );
    st.purge(&without_account(&s, "b"), 2_000);
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], 3_000),
                &mut r
            )
            .0
        ),
        "a",
        "the purged cursor restarted at index 0 instead of resuming on b"
    );
}

#[test]
fn a_sample_that_retires_overage_evidence_returns_an_account_to_the_plan_funded_group() {
    let s = snapshot(Pool {
        policy: OveragePolicy::Allow,
        strategy: Strategy::Priority,
        prefer_subscription: true,
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let pick = |st: &mut RoutingState, r: &mut StdRng, at: i64| {
        picked(
            &select(
                st,
                &s,
                &request(&s, None, "claude-sonnet-5", true, &[], at),
                r,
            )
            .0,
        )
        .to_string()
    };
    let sampled = |in_use: bool, observed_at: i64| OverageEvidence {
        in_use,
        since: Some(observed_at),
        reset: None,
        observed_at,
        source: EvidenceSource::Sample,
        incarnation: 1,
    };
    st.record_overage("a", sampled(true, 1_000));
    let mut served = std::collections::HashSet::new();
    for i in 0..8 {
        served.insert(pick(&mut st, &mut r, 1_000 + i));
    }
    assert_eq!(
        served,
        ["b".to_string()].into_iter().collect(),
        "the account in overage sits one group below the plan-funded one while the evidence stands"
    );
    // `in_overage` never expires evidence of its own and the quota poll writes the ledger, not routing state,
    // so this sample is the only thing that can retire it. `needs_sample` has to ask for it.
    st.record_overage("a", sampled(false, 2_000));
    assert!(!st.in_overage("a", 2_000, FALLBACK_MS));
    let mut served = std::collections::HashSet::new();
    for i in 0..20 {
        served.insert(pick(&mut st, &mut r, 2_000 + i));
    }
    assert_eq!(
        served.len(),
        2,
        "the recovered account shares the first group again: {served:?}"
    );
}

#[test]
fn every_candidate_cooled_for_a_rejection_reports_model_rejected() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled(
        "a",
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    st.record_cooldown(cooled(
        "b",
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-4-5", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(result, SelectResult::Exhausted(Exhaustion::ModelRejected));
    let (other, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert!(
        matches!(other, SelectResult::Selected(_)),
        "the rejection is scoped to the other model, so both siblings are still eligible"
    );
}

#[test]
fn a_rejection_beside_a_rate_limit_reports_the_rate_limits_deadline_and_window() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let scenario = |rejection_until: i64| {
        let mut st = RoutingState::from_load(RoutingLoad::default());
        st.record_cooldown(cooled(
            "a",
            "model:claude-sonnet-4-5",
            rejection_until,
            "model_rejection",
            MODEL_REJECTED_REASON,
        ));
        st.record_cooldown(cooled(
            "b",
            "account",
            1_000 + 2 * HOUR,
            "five_hour",
            "rate_limit",
        ));
        select(
            &mut st,
            &s,
            &request(&s, None, "claude-sonnet-4-5", true, &[], 1_000),
            &mut rng(),
        )
        .0
    };
    assert!(
        matches!(scenario(1_000 + 3 * HOUR), SelectResult::Exhausted(Exhaustion::Timed { deadline, ref window }) if deadline == 1_000 + 2 * HOUR && window == "five_hour"),
        "the account's own rate limit is the wait, whatever the rejection's own deadline"
    );
    assert!(
        matches!(scenario(1_000 + HOUR), SelectResult::Exhausted(Exhaustion::Timed { deadline, ref window }) if deadline == 1_000 + 2 * HOUR && window == "five_hour"),
        "the rejection expiring first never dates the answer or names its own window"
    );
}

#[test]
fn a_paused_member_beside_a_rejection_still_reports_model_rejected() {
    let s = snapshot_with(
        Pool {
            members: vec![("a", 0), ("b", 0)],
            ..Pool::default()
        },
        &[("b", false, AccountState::Ready)],
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled(
        "a",
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-4-5", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(result, SelectResult::Exhausted(Exhaustion::ModelRejected));
}

#[test]
fn a_non_portable_request_whose_pinned_member_is_cooling_for_a_rejection_reports_model_rejected() {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let first = picked(
        &select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-4-5", false, &[], 1_000),
            &mut r,
        )
        .0,
    )
    .to_string();
    st.record_cooldown(cooled(
        &first,
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    let (result, writes) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-4-5", false, &[], 2_000),
        &mut r,
    );
    assert_eq!(result, SelectResult::Exhausted(Exhaustion::ModelRejected));
    assert!(
        writes.is_empty(),
        "the pin stays, the same as any other blocked pin"
    );
    st.record_cooldown(cooled(
        &first,
        "account",
        3_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-4-5", false, &[], 3_000),
        &mut r,
    );
    assert!(
        matches!(result, SelectResult::Exhausted(Exhaustion::Timed { deadline, ref window }) if deadline == 3_000 + HOUR && window == "five_hour"),
        "a rate limit on the pinned account is still a wait the caller can arm on"
    );
}

#[test]
fn a_pin_under_the_folded_spelling_holds_its_member_for_a_non_portable_turn_after_the_resolution() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let SelectResult::Selected(opening) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-4-5", true, &[], 1_000),
        &mut r,
    )
    .0
    else {
        panic!()
    };
    assert_eq!(opening.account.id, "a");
    let epoch = opening.pin.unwrap().epoch;
    let rekey = st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 2_000);
    assert_eq!(
        rekey.delete_pins,
        [(
            "k".to_string(),
            "s1".to_string(),
            "claude-sonnet-4-5".to_string()
        )],
        "the pin under the folded spelling is dropped"
    );
    assert!(
        rekey
            .upsert_pins
            .iter()
            .any(|p| p.model == "claude-sonnet-4-5-20250929"),
        "and re-written under the dated id: {rekey:?}"
    );
    assert_eq!(
        (
            rekey.resolution.spelling.as_str(),
            rekey.resolution.dated_id.as_str()
        ),
        ("claude-sonnet-4-5", "claude-sonnet-4-5-20250929"),
        "and the resolution itself travels with them"
    );
    let SelectResult::Selected(sel) = select(
        &mut st,
        &s,
        &request(
            &s,
            Some("s1"),
            "claude-sonnet-4-5-20250929",
            false,
            &[],
            3_000,
        ),
        &mut r,
    )
    .0
    else {
        panic!()
    };
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.migrated,
            sel.pin.unwrap().epoch
        ),
        ("a", None, epoch),
        "the pin renews under the dated id rather than starting over"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-4-5", false, &[], 4_000),
                &mut r,
            )
            .0
        ),
        "a",
        "the folded spelling still finds the same pin"
    );
}

#[test]
fn a_resolution_moves_every_row_in_one_rekey_and_a_repeat_carries_only_the_resolution() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let SelectResult::Selected(_) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-4-5", true, &[], 1_000),
        &mut r,
    )
    .0
    else {
        panic!()
    };
    st.record_cooldown(cooled(
        "b",
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    let first = st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 2_000);
    assert_eq!(
        (
            first.delete_pins.len(),
            first
                .upsert_pins
                .iter()
                .map(|p| p.model.as_str())
                .collect::<Vec<_>>()
        ),
        (1, vec!["claude-sonnet-4-5-20250929"]),
        "the pin moves: {first:?}"
    );
    assert_eq!(
        (
            first.delete_cooldowns.clone(),
            first
                .upsert_cooldowns
                .iter()
                .map(|c| (c.account_id.as_str(), c.scope.as_str()))
                .collect::<Vec<_>>()
        ),
        (
            vec![("b".to_string(), "model:claude-sonnet-4-5".to_string())],
            vec![("b", "model:claude-sonnet-4-5-20250929")]
        ),
        "and the cooldown with it: {first:?}"
    );
    assert_eq!(first.resolution.resolved_at, 2_000);
    let second = st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 3_000);
    assert!(
        second.delete_pins.is_empty()
            && second.upsert_pins.is_empty()
            && second.delete_cooldowns.is_empty()
            && second.upsert_cooldowns.is_empty(),
        "a repeat moves nothing that already moved: {second:?}"
    );
    assert_eq!(
        (
            second.resolution.dated_id.as_str(),
            second.resolution.resolved_at
        ),
        ("claude-sonnet-4-5-20250929", 3_000),
        "and carries the resolution row alone"
    );
}

#[test]
fn a_resolution_never_moves_a_spelling_to_a_dated_id_that_is_not_newer() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut r = rng();
    let SelectResult::Selected(_) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-4-5", true, &[], 1_000),
        &mut r,
    )
    .0
    else {
        panic!()
    };
    st.record_cooldown(cooled(
        "b",
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 2_000);
    let regressed = st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250601", 3_000);
    assert!(
        regressed.delete_pins.is_empty()
            && regressed.upsert_pins.is_empty()
            && regressed.delete_cooldowns.is_empty()
            && regressed.upsert_cooldowns.is_empty(),
        "an older snapshot moves nothing: {regressed:?}"
    );
    assert_eq!(
        (
            regressed.resolution.dated_id.as_str(),
            regressed.resolution.resolved_at
        ),
        ("claude-sonnet-4-5-20250929", 3_000),
        "the mapping stays at the newer snapshot"
    );
    assert_eq!(
        st.canonical("claude-sonnet-4-5"),
        "claude-sonnet-4-5-20250929",
        "a lookup still answers the newer snapshot"
    );
}

#[test]
fn a_rejection_cooldown_under_the_folded_spelling_still_excludes_the_member_under_the_dated_id() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled(
        "a",
        "model:claude-sonnet-4-5",
        1_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", 500);
    let (result, _) = select(
        &mut st,
        &s,
        &request(&s, None, "claude-sonnet-4-5-20250929", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(picked(&result), "b");
    let solo = SelectRequest {
        candidates: resolve(
            &s,
            s.key("k").unwrap(),
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5-20250929",
        )
        .unwrap()
        .into_iter()
        .filter(|c| c.account_id != "b")
        .collect(),
        ..request(&s, None, "claude-sonnet-4-5-20250929", true, &[], 1_000)
    };
    let (result, _) = select(&mut st, &s, &solo, &mut rng());
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::ModelRejected),
        "no second attempt on the cooled member"
    );
    st.record_cooldown(cooled(
        "a",
        "model:claude-sonnet-4-5",
        2_000 + HOUR,
        "model_rejection",
        MODEL_REJECTED_REASON,
    ));
    assert_eq!(
        st.cooldown("a", "claude-sonnet-4-5-20250929", 1_000)
            .map(|c| c.until),
        Some(2_000 + HOUR),
        "a cooldown recorded under the folded scope after the landing is stored under the dated scope"
    );
}

#[test]
fn an_overage_whose_named_reset_has_passed_no_longer_counts_for_the_tier() {
    let s = snapshot(Pool {
        prefer_subscription: true,
        members: vec![("a", 5), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_overage("a", overage(true, 1_000, Some(999)));
    st.record_overage("b", overage(true, 1_000, Some(2_000)));
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "a",
        "a's evidence no longer stands, so it sits in the plan tier ahead of b's overage tier despite the worse priority"
    );

    // Evidence naming no reset stands for the request's fallback and then not.
    st.record_overage("b", overage(true, 1_500, None));
    let still = request(
        &s,
        None,
        "claude-sonnet-5",
        true,
        &[],
        1_500 + FALLBACK_MS - 1,
    );
    assert_eq!(
        picked(&select(&mut st, &s, &still, &mut rng()).0),
        "a",
        "b's fallback has not passed, so it still sits below a"
    );
    let expired = request(
        &s,
        None,
        "claude-sonnet-5",
        true,
        &[],
        1_500 + FALLBACK_MS + 1,
    );
    assert_eq!(
        picked(&select(&mut st, &s, &expired, &mut rng()).0),
        "b",
        "b's fallback has passed, so it now outranks a on priority alone"
    );
}

#[test]
fn an_overage_whose_named_reset_has_passed_no_longer_counts_for_the_move() {
    let s = snapshot(Pool {
        policy: OveragePolicy::PreferPlan,
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pick = |st: &mut RoutingState, session: &str, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some(session), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&pick(&mut st, "s1", 1_000).0), "a");
    st.record_overage("a", overage(true, 1_000, Some(1_999)));
    let SelectResult::Selected(sel) = pick(&mut st, "s1", 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "the named reset is behind now, so a's evidence no longer stands and the pin renews rather than moving"
    );

    // Evidence naming no reset stands for the request's fallback and then not.
    assert_eq!(picked(&pick(&mut st, "s2", 3_000).0), "a");
    st.record_overage("a", overage(true, 3_000, None));
    let SelectResult::Selected(sel) = pick(&mut st, "s2", 3_000 + FALLBACK_MS - 1).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Overage)),
        "inside the fallback a is still leaving overage"
    );

    assert_eq!(picked(&pick(&mut st, "s3", 4_000).0), "a");
    st.record_overage("a", overage(true, 4_000, None));
    let SelectResult::Selected(sel) = pick(&mut st, "s3", 4_000 + FALLBACK_MS + 1).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("a", None),
        "past the fallback a's evidence no longer stands, so its pin renews too"
    );
}

#[test]
fn an_overage_whose_named_reset_has_passed_no_longer_counts_for_the_release_reasons() {
    let s = snapshot(Pool {
        policy: OveragePolicy::PreferPlan,
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let pin = PinRow {
        key_id: "k".into(),
        session_id: "s1".into(),
        model: "claude-sonnet-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 0,
        last_used_at: 0,
        key_generation: 0,
        incarnation: 1,
        burn: 0.0,
        burn_at: None,
        last_input_tokens: 0,
        last_refresh_at: 0,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
    };
    st.record_overage("a", overage(true, 1_000, Some(1_999)));
    let req = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000);
    assert!(
        !release_reasons(&st, &s, &pin, &req).contains(&"overage"),
        "the named reset is behind now, so a's evidence no longer stands"
    );

    // Evidence naming no reset stands for the request's fallback and then not.
    st.record_overage("a", overage(true, 2_000, None));
    let still = request(
        &s,
        Some("s1"),
        "claude-sonnet-5",
        true,
        &[],
        2_000 + FALLBACK_MS - 1,
    );
    assert!(
        release_reasons(&st, &s, &pin, &still).contains(&"overage"),
        "inside the fallback the evidence still stands"
    );
    let expired = request(
        &s,
        Some("s1"),
        "claude-sonnet-5",
        true,
        &[],
        2_000 + FALLBACK_MS + 1,
    );
    assert!(!release_reasons(&st, &s, &pin, &expired).contains(&"overage"));
}

#[test]
fn pace_reads_the_window_map_and_ranks_an_api_key_account_by_its_worst_bucket() {
    let s = snapshot(Pool {
        strategy: Strategy::Pace,
        kinds: vec![("a", AccountKind::ApiKey), ("b", AccountKind::ApiKey)],
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let bucket = |utilization: f64, reset: Option<i64>, observed_at: i64| {
        UtilizationSample {
            observed_at,
            ..Default::default()
        }
        .with(
            "itpm:x",
            WindowReading {
                utilization,
                reset,
                observed_at,
            },
        )
    };
    st.record_sample("a", bucket(0.3, Some(5_000), 1_000));
    st.record_sample("b", bucket(0.6, Some(1_000_000), 1_000));
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    for seed in 0..8u64 {
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0),
            "a",
            "0.3 utilization beats 0.6 utilization whatever the resets, since a bucket's own reset never enters the score"
        );
    }

    // Two at the same utilization with different resets draw at random over a seeded rng, both seen.
    st.record_sample("a", bucket(0.5, Some(5_000), 2_000));
    st.record_sample("b", bucket(0.5, Some(9_000_000), 2_000));
    let mut seen = std::collections::HashSet::new();
    for seed in 0..16u64 {
        seen.insert(
            picked(&select(&mut st, &s, &req, &mut StdRng::seed_from_u64(seed)).0).to_string(),
        );
    }
    assert_eq!(seen.len(), 2, "both accounts are seen: {seen:?}");
}

#[test]
fn a_subscription_window_past_its_reset_scores_as_untouched_not_by_a_minutes_clamp() {
    let s = snapshot(Pool {
        strategy: Strategy::Pace,
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // a's shared window is exhausted with a reset a moment behind now: the old clamp-to-a-minute arm ranked
    // this first on remaining over an inflated score; live_at instead reads it as untouched, on the full week.
    st.record_sample(
        "a",
        UtilizationSample {
            observed_at: 999,
            ..Default::default()
        }
        .with(
            SEVEN_DAY,
            WindowReading {
                utilization: 1.0,
                reset: Some(999),
                observed_at: 999,
            },
        ),
    );
    // b holds real remaining allowance resetting soon, which would lose to a's clamp-inflated score.
    st.record_sample(
        "b",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.9,
                reset: Some(1_000 + HOUR),
                observed_at: 1_000,
            },
        ),
    );
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "b",
        "a reads as untouched (1/168 \u{2248} 0.006), which loses to b's real remaining over an hour (0.1/1 = 0.1)"
    );
}

#[test]
fn the_five_hour_ceiling_demotes_an_account_under_pace() {
    let s = snapshot(Pool {
        strategy: Strategy::Pace,
        select_below: Some(0.9),
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // a: above the ceiling but the best Pace score (almost nothing used).
    st.record_sample("a", sample(0.95, 0.01, 1_000));
    // b: below the ceiling but a worse Pace score (more used).
    st.record_sample("b", sample(0.5, 0.5, 1_000));
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "b",
        "the ceiling prefers b even though a scores better under pace"
    );

    // With no sibling below the ceiling, a is not stranded.
    let solo = snapshot(Pool {
        strategy: Strategy::Pace,
        select_below: Some(0.9),
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    st2.record_sample("a", sample(0.95, 0.01, 1_000));
    let req2 = request(&solo, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        picked(&select(&mut st2, &solo, &req2, &mut rng()).0),
        "a",
        "and a wins once no sibling is below it"
    );
}

#[test]
fn an_exhausted_bucket_of_one_model_is_refunded_at_its_reset_and_a_siblings_free_capacity_ranks_the_account(
) {
    let s = snapshot(Pool {
        strategy: Strategy::Pace,
        kinds: vec![("a", AccountKind::ApiKey), ("b", AccountKind::ApiKey)],
        members: vec![("a", 0), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample(
        "a",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            "itpm:x",
            WindowReading {
                utilization: 1.0,
                reset: Some(1_030),
                observed_at: 1_000,
            },
        ),
    );
    st.record_sample(
        "a",
        UtilizationSample {
            observed_at: 1_031,
            ..Default::default()
        }
        .with(
            "itpm:y",
            WindowReading {
                utilization: 0.1,
                reset: None,
                observed_at: 1_031,
            },
        ),
    );
    st.record_sample(
        "b",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            "itpm:z",
            WindowReading {
                utilization: 0.5,
                reset: None,
                observed_at: 1_000,
            },
        ),
    );
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_031);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "a",
        "x is refunded at its reset, so a ranks by y's 0.9 remaining, above b's 0.5 and above the floor"
    );
}

#[test]
fn the_pace_floor_makes_an_account_last_resort_and_places_by_pace_inside_the_floor_group() {
    let s = snapshot(Pool {
        strategy: Strategy::Pace,
        members: vec![("a", 0), ("b", 0), ("c", 0)],
        failover: Failover {
            pace_floor: 0.1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", sample(0.0, 0.95, 1_000)); // remaining 0.05, below the floor
    st.record_sample("b", sample(0.0, 0.92, 1_000)); // remaining 0.08, below the floor
    st.record_sample("c", sample(0.0, 0.5, 1_000)); // remaining 0.5, above the floor
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "c",
        "an almost-spent account is kept for when nothing else can serve, so c places first"
    );

    // All three below the floor: the floor partition keeps the wider set, and pace ranks among them.
    st.record_sample("c", sample(0.0, 0.97, 1_100)); // remaining 0.03, now below the floor too
    let req2 = request(&s, None, "claude-sonnet-5", true, &[], 1_100);
    assert_eq!(
        picked(&select(&mut st, &s, &req2, &mut rng()).0),
        "b",
        "nothing is above the floor, so pace places b, the least spent of the three"
    );
}

#[test]
fn an_ending_account_is_paced_against_the_earlier_of_its_reset_and_its_end() {
    let s = snapshot_with_terms(
        Pool {
            strategy: Strategy::Pace,
            members: vec![("a", 0), ("b", 0)],
            ..Pool::default()
        },
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(1_000 + 6 * HOUR),
        }],
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample(
        "a",
        sample(0.0, 0.5, 1_000).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.5,
                reset: Some(1_000 + 96 * HOUR),
                observed_at: 1_000,
            },
        ),
    );
    st.record_sample(
        "b",
        sample(0.0, 0.5, 1_000).with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.5,
                reset: Some(1_000 + 24 * HOUR),
                observed_at: 1_000,
            },
        ),
    );
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "a",
        "a's end at 6h outranks its own 96h reset and b's 24h reset alike"
    );
}

#[test]
fn a_winding_down_account_takes_a_fresh_session_only_as_the_last_resort_under_every_strategy() {
    for strategy in [Strategy::Pace, Strategy::Priority, Strategy::RoundRobin] {
        let term = vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(1_000 + 10 * 60_000), // 10 minutes out, inside the 30-minute window
        }];
        let failover = Failover {
            wind_down_minutes: 30,
            ..Failover::default()
        };
        let s = snapshot_with_terms(
            Pool {
                strategy,
                members: vec![("a", 0), ("b", 0)],
                failover,
                ..Pool::default()
            },
            term.clone(),
        );
        let mut st = RoutingState::from_load(RoutingLoad::default());
        let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut rng()).0),
            "b",
            "{strategy:?}: a is winding down, so a fresh session prefers b"
        );

        // With no sibling, the winding-down account still serves rather than the request going unserved.
        let solo = snapshot_with_terms(
            Pool {
                strategy,
                members: vec![("a", 0)],
                failover,
                ..Pool::default()
            },
            term,
        );
        let mut st2 = RoutingState::from_load(RoutingLoad::default());
        let req2 = request(&solo, None, "claude-sonnet-5", true, &[], 1_000);
        assert_eq!(
            picked(&select(&mut st2, &solo, &req2, &mut rng()).0),
            "a",
            "{strategy:?}: as the last resort a still serves"
        );
    }
}

#[test]
fn a_cancelled_account_past_its_end_is_an_obstacle_and_its_pin_is_released_as_subscription_ended() {
    let s = snapshot_with_terms(
        Pool {
            members: vec![("a", 0), ("b", 1)],
            // Isolated from the wind-down partition, which would otherwise demote a on its own well before
            // its end.
            failover: Failover {
                wind_down_minutes: 0,
                ..Failover::default()
            },
            ..Pool::default()
        },
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(5_000),
        }],
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
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
        "a",
        "a still serves before its end"
    );
    let pin = st
        .pin("k", "s1", "claude-sonnet-5")
        .cloned()
        .expect("the first select pinned a");
    let after_end = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 6_000);
    assert!(
        release_reasons(&st, &s, &pin, &after_end).contains(&"subscription_ended"),
        "past its saved end, a is the obstacle subscription_ended"
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &after_end, &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        sel.account.id, "b",
        "the pin moves off a once it can no longer serve"
    );
}

#[test]
fn a_future_trial_end_counts_as_an_end_and_a_passed_one_leaves_the_account_alone() {
    let pool = || Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            wind_down_minutes: 30,
            ..Failover::default()
        },
        ..Pool::default()
    };
    let future = snapshot_with_terms(
        pool(),
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: Some(1_000 + 10 * 60_000),
            cancelled: false,
            ends_at: None,
        }],
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(&select(&mut st, &future, &request(&future, None, "claude-sonnet-5", true, &[], 1_000), &mut rng()).0),
        "b",
        "a's trial end is still ahead, so it counts and a fresh session prefers b despite the worse priority"
    );

    let past = snapshot_with_terms(
        pool(),
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: Some(500),
            cancelled: false,
            ends_at: None,
        }],
    );
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st2,
                &past,
                &request(&past, None, "claude-sonnet-5", true, &[], 1_000),
                &mut rng()
            )
            .0
        ),
        "a",
        "a's trial end has passed, so it no longer counts and a wins on priority as usual"
    );
}

#[test]
fn a_fresh_placement_prefers_an_account_under_its_in_flight_cap_and_the_trace_names_the_skipped_ones(
) {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 0)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let ticket = st.in_flight().dispatched("a", 1_000);
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        sel.account.id, "b",
        "a is at its cap, so a fresh placement prefers b"
    );
    assert!(
        sel.trace
            .skipped
            .iter()
            .any(|s| s.account_id == "a" && s.obstacle == "at_cap"),
        "{:?}",
        sel.trace.skipped
    );
    drop(ticket);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        sel.account.id, "a",
        "the ticket dropped, so a is under its cap again"
    );
}
