use phoebis_routing::{
    resolve, select, EvidenceSource, Exhaustion, MigrateReason, OverageEvidence, PinRef,
    RegistrySnapshot, RoutingState, SelectRequest, SelectResult, StateWrite, UtilizationSample,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, ModelSource, OveragePolicy,
    PoolMember, PoolRow, RegistryLoad, RoutingLoad, ServedModel, Strategy,
};
use rand::rngs::StdRng;
use rand::SeedableRng;

const HOUR: i64 = 3_600_000;

struct Pool {
    policy: OveragePolicy,
    strategy: Strategy,
    affinity: u64,
    migrate: Option<f64>,
    members: Vec<(&'static str, u32)>,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            policy: OveragePolicy::PreferPlan,
            strategy: Strategy::PriorityRandom,
            affinity: 3600,
            migrate: None,
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
            .flat_map(|a| [served(a, "claude-sonnet-5"), served(a, "claude-opus-5")])
            .collect(),
        pools: vec![PoolRow {
            id: "p".into(),
            label: "p".into(),
            enabled: true,
            strategy: pool.strategy,
            affinity_ttl_seconds: pool.affinity,
            overage_policy: pool.policy,
            migrate_above_utilization: pool.migrate,
            default_allowed_models: None,
            keepalive_json: "{}".into(),
            body_capture_hours: 0,
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
        five_hour: Some(five_hour),
        seven_day: Some(seven_day),
        observed_at,
    }
}

fn picked(result: &SelectResult) -> &str {
    match result {
        SelectResult::Selected(s) => &s.account_id,
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
        (sel.account_id.as_str(), sel.migrated),
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
        (sel.account_id.as_str(), sel.migrated),
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
    let first = picked(
        &select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 0),
            &mut r,
        )
        .0,
    )
    .to_string();
    for at in [3_500_000, 7_000_000] {
        assert_eq!(
            picked(
                &select(
                    &mut st,
                    &s,
                    &request(&s, Some("s1"), "claude-sonnet-5", true, &[], at),
                    &mut r
                )
                .0
            ),
            first
        );
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
        !st.invalidate_pin(&other, &sel.account_id),
        "a newer epoch is another pin"
    );
    assert!(
        !st.invalidate_pin(&pin, "zzz"),
        "another account's failure leaves it alone"
    );
    assert!(st.invalidate_pin(&pin, &sel.account_id));
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
    for i in 0..10 {
        req.now = i;
        assert_eq!(
            picked(&select(&mut st, &s, &req, &mut rng()).0),
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
