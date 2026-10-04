use phoebis_routing::{
    cohort_hold, effective_warm, lane_key, release_reasons, resolve, select, warm, warmth_of,
    AffinityChain, AffinityMap, AffinityRow, BurnRates, BurnUnit, ClientClass, CohortHold,
    CreditsState, EvidenceSource, Exhaustion, HoldFacts, MemberSpend,
    MigrateReason, OverageEvidence, PinRef, Placement, RateTable, Rates, RegistrySnapshot,
    RoutingState, SelectRequest, SelectResult, StateWrite, UtilizationSample, Warmth,
    WindowReading, FAST_CREDITS_REASON, FAST_RATE_LIMIT_REASON, FIVE_HOUR, MODEL_REJECTED_REASON,
    SEVEN_DAY, SEVEN_DAY_FABLE, WEEK_MS,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Affinity, AffinityMode, ApiKeyRow, BudgetPeriod,
    CooldownRow, Failover, OveragePolicy, PinBurn, PinKey, PinRow, PoolMember,
    PoolRow, RegistryLoad, RoutingLoad, RulesMode, SessionPlacement, Shedding, Strategy,
    SubagentPlacement, SubscriptionTerm, WarmSessions,
};
use rand::rngs::StdRng;
use rand::SeedableRng;

const HOUR: i64 = 3_600_000;
/// The fallback `request()` carries, for a call to the overage predicate outside a full `SelectRequest`.
const FALLBACK_MS: i64 = 60_000;

struct Pool {
    policy: OveragePolicy,
    strategy: Strategy,
    affinity: u64,
    mode: AffinityMode,
    subagent_placement: SubagentPlacement,
    session_placement: SessionPlacement,
    min_shared_prefix_tokens: u64,
    migrate: Option<f64>,
    select_below: Option<f64>,
    members: Vec<(&'static str, u32)>,
    prefer_subscription: bool,
    kinds: Vec<(&'static str, AccountKind)>,
    shedding: Shedding,
    failover: Failover,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            policy: OveragePolicy::PreferPlan,
            strategy: Strategy::Priority,
            affinity: 3600,
            mode: AffinityMode::Always,
            subagent_placement: SubagentPlacement::ByCohort,
            session_placement: SessionPlacement::ByStrategy,
            min_shared_prefix_tokens: 20_000,
            migrate: None,
            select_below: None,
            members: vec![("a", 0), ("b", 0), ("c", 0)],
            prefer_subscription: false,
            kinds: vec![],
            shedding: Shedding::default(),
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
            auto_mode_server: None,
        }],
        members: pool
            .members
            .iter()
            .map(|(a, p)| PoolMember {
                pool_id: "p".into(),
                account_id: (*a).into(),
                priority: *p,
                budget_daily_usd: None,
                budget_monthly_usd: None,
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
            shedding: pool.shedding,
            failover: pool.failover,
            cache_ttl_json: "{}".into(),
            cache_diagnostics: false,
            auto_mode_server: true,
            affinity: Affinity {
                mode: pool.mode,
                subagent_placement: pool.subagent_placement,
                session_placement: pool.session_placement,
                min_shared_prefix_tokens: pool.min_shared_prefix_tokens,
            },
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

/// `s` rebuilt with `account_id`'s pool membership carrying the daily and monthly budgets, the way the admin
/// route's `set_pool_members` would leave it.
fn with_member_budget(
    s: &RegistrySnapshot,
    account_id: &str,
    daily: Option<f64>,
    monthly: Option<f64>,
) -> RegistrySnapshot {
    let load = RegistryLoad {
        accounts: s.accounts().cloned().collect(),
        credential_generations: Vec::new(),
        keys: s.keys().cloned().collect(),
        members: s
            .pools()
            .flat_map(|p| s.members(&p.id).to_vec())
            .map(|mut m| {
                if m.account_id == account_id {
                    m.budget_daily_usd = daily;
                    m.budget_monthly_usd = monthly;
                }
                m
            })
            .collect(),
        pools: s.pools().cloned().collect(),
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
        terms: Vec::new(),
    };
    RegistrySnapshot::from_load(load, [0; 32], 0, 1)
}

/// One affinity row under key "k" at its current generation and account `account` under incarnation 1, for
/// `state.affinity().write()` to seed directly.
#[allow(clippy::too_many_arguments)]
fn affinity_row(
    s: &RegistrySnapshot,
    model: &str,
    hash: &str,
    chain: AffinityChain,
    account: &str,
    written_at: i64,
    ttl_ms: i64,
    prefix_tokens: u64,
    root: &str,
) -> AffinityRow {
    AffinityRow {
        model: model.into(),
        hash: hash.into(),
        chain,
        account_id: account.into(),
        incarnation: 1,
        written_at,
        ttl_ms,
        prefix_tokens,
        key_id: "k".into(),
        key_generation: s.key("k").unwrap().created_at,
        root: root.into(),
        lane: lane_key(model, "standard", "none", "-"),
    }
}

/// Replaces the state's affinity map whole, the way a placement test seeds exact entries without going through
/// a report's own upsert rules.
fn seed_affinity(st: &RoutingState, rows: Vec<AffinityRow>) {
    *st.affinity().write().unwrap() = AffinityMap::from_rows(rows);
}

/// `req` carrying its full message and static chains and the cache-gate marker fact, for a placement test.
fn placement_req(
    mut req: SelectRequest,
    boundaries: &[&str],
    static_boundaries: &[&str],
    marker: bool,
) -> SelectRequest {
    req.boundaries = boundaries.iter().map(|b| b.to_string()).collect();
    req.static_boundaries = static_boundaries.iter().map(|b| b.to_string()).collect();
    req.static_hash = static_boundaries.last().map(|b| b.to_string());
    req.marker = marker;
    req
}

/// `req` under a subagent's own id, for a placement test.
fn subagent(mut req: SelectRequest, agent_id: &str) -> SelectRequest {
    req.agent_id = agent_id.into();
    req
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
        agent_id: "main".into(),
        session_id: session.map(String::from),
        canonical_model: model.into(),
        candidates: resolve(s, s.key("k").unwrap(), model, model).unwrap(),
        portable,
        attempted: attempted.iter().map(|a| a.to_string()).collect(),
        now,
        fallback_ms: 60_000,
        revision: s.revision,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        static_hash: None,
        boundaries: Vec::new(),
        client: ClientClass::ClaudeCode,
        static_boundaries: Vec::new(),
        marker: false,
        parent_agent_id: None,
        holds: Vec::new(),
        lane: lane_key(model, "standard", "none", "-"),
        fast: false,
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
fn a_subagent_and_the_main_agent_of_one_session_pin_separately() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let main = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000);
    let sub = SelectRequest {
        agent_id: "sub-1".into(),
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000)
    };
    select(&mut st, &s, &main, &mut rng());
    select(&mut st, &s, &sub, &mut rng());
    assert_eq!(
        st.pins().len(),
        2,
        "the main agent and its subagent each hold their own pin"
    );
    let main_key = pin_key("s1");
    let sub_key = PinKey {
        agent_id: "sub-1".into(),
        ..pin_key("s1")
    };
    let main_epoch = st.pin(&main_key).unwrap().epoch;
    let sub_last_used = st.pin(&sub_key).unwrap().last_used_at;
    let renewal = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000);
    select(&mut st, &s, &renewal, &mut rng());
    assert_eq!(
        (
            st.pin(&main_key).unwrap().epoch,
            st.pin(&main_key).unwrap().last_used_at
        ),
        (main_epoch, 2_000),
        "the third select renews the main agent's pin alone"
    );
    assert_eq!(
        st.pin(&sub_key).unwrap().last_used_at,
        sub_last_used,
        "the subagent's pin is untouched by the main agent's renewal"
    );
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
fn a_marked_pin_moves_to_a_cooler_sibling_and_stays_when_none_exists() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    // Two cold pins, so the round has one to spare: it marks the hotter.
    let mut st = loaded(vec![
        burning("s1", "a", 4.0, false, T),
        burning("s0", "a", 1.0, false, T),
    ]);
    st.record_sample("a", sample(0.85, 0.3, T - 500));
    st.record_sample("b", sample(0.9, 0.3, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), None),
        "no cooler sibling, the marked pin stays"
    );
    st.record_sample("b", sample(0.1, 0.3, T + 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "the cooler sibling wins even at the worse priority"
    );
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 2_000), &mut rng()).0),
        ("b".into(), None),
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
    // Past the overage grace, so the move does not wait for the plan to come back.
    st.record_overage("a", overage(true, 1_500, Some(9 * HOUR)));
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
fn a_spent_fable_window_that_dated_no_reset_keeps_excluding_until_a_newer_reading_lands() {
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
                1_000 + 6 * HOUR
            ),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Untimed),
        "a full reading that names no reset stands however old it is"
    );
    st.record_sample("a", with_fable(sample(0.1, 0.1, 2_000), 0.5, None));
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
                    1_000 + 6 * HOUR
                ),
                &mut rng()
            )
            .0
        ),
        "a",
        "a newer reading under the limit ends the exclusion"
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
    st.record_overage("a", overage(true, 1_500, Some(9 * HOUR)));
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
        !writes.iter().any(|w| matches!(w, StateWrite::DeletePin(_))),
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
        writes.iter().any(|w| matches!(w, StateWrite::DeletePin(_))),
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

/// `req` sent with `speed: "fast"`, on its model's fast lane.
fn fast(req: SelectRequest) -> SelectRequest {
    SelectRequest {
        fast: true,
        lane: lane_key(&req.canonical_model, "fast", "none", "-"),
        ..req
    }
}

/// The poll's extra-usage state, its utilization a fraction.
fn credits(
    enabled: bool,
    reason: Option<&str>,
    utilization: Option<f64>,
    observed_at: i64,
) -> CreditsState {
    CreditsState {
        enabled,
        disabled_reason: reason.map(String::from),
        utilization,
        observed_at,
        incarnation: 1,
    }
}

fn skipped_as(result: &SelectResult) -> Vec<(&str, &str)> {
    match result {
        SelectResult::Selected(s) => s
            .trace
            .skipped
            .iter()
            .map(|k| (k.account_id.as_str(), k.obstacle))
            .collect(),
        other => panic!("expected a selection, got {other:?}"),
    }
}

#[test]
fn a_fast_request_passes_a_plan_window_cooldown_and_is_blocked_by_a_fast_a_model_or_a_retry_after_row(
) {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled("a", "account", HOUR, "five_hour", "rate_limit"));
    st.record_cooldown(cooled("b", "fast", HOUR, "fast", FAST_RATE_LIMIT_REASON));
    st.record_cooldown(cooled(
        "c",
        "model:claude-opus-5",
        HOUR,
        "retry_after",
        "rate_limit",
    ));
    let at = |now: i64| fast(request(&s, None, "claude-opus-5", true, &[], now));
    let (result, _) = select(&mut st, &s, &at(1_000), &mut rng());
    assert_eq!(
        picked(&result),
        "a",
        "a fast request draws no plan window, so the five-hour row is no obstacle"
    );
    assert_eq!(skipped_as(&result), [("b", "cooldown"), ("c", "cooldown")]);
    for plan in ["seven_day", "seven_day_opus"] {
        let mut st = RoutingState::from_load(RoutingLoad::default());
        st.record_cooldown(cooled("a", "account", HOUR, plan, "rate_limit"));
        st.record_cooldown(cooled("b", "account", HOUR, plan, "rate_limit"));
        st.record_cooldown(cooled("c", "account", HOUR, plan, "rate_limit"));
        assert!(
            matches!(
                select(&mut st, &s, &at(1_000), &mut rng()).0,
                SelectResult::Selected(_)
            ),
            "{plan}"
        );
    }
    st.record_cooldown(cooled(
        "a",
        "account",
        2 * HOUR,
        "retry_after",
        "rate_limit",
    ));
    assert!(
        matches!(
            select(&mut st, &s, &at(1_000), &mut rng()).0,
            SelectResult::Exhausted(_)
        ),
        "an account row no plan window names holds either speed"
    );
}

#[test]
fn a_standard_request_passes_no_fast_row_and_is_blocked_by_every_row_it_was_before() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1), ("c", 2)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64| request(&s, None, "claude-opus-5", true, &[], now);
    st.record_cooldown(cooled("a", "fast", HOUR, "fast", FAST_RATE_LIMIT_REASON));
    st.record_cooldown(cooled(
        "b",
        "fast_credits",
        HOUR,
        "out_of_credits",
        FAST_CREDITS_REASON,
    ));
    st.record_credits("a", credits(false, Some("out_of_credits"), None, 500));
    assert_eq!(
        picked(&select(&mut st, &s, &at(1_000), &mut rng()).0),
        "a",
        "neither a fast row nor the credits state holds a standard request"
    );
    st.record_cooldown(cooled("a", "account", HOUR, "five_hour", "rate_limit"));
    assert_eq!(picked(&select(&mut st, &s, &at(1_000), &mut rng()).0), "b");
    st.record_cooldown(cooled(
        "b",
        "model:claude-opus-5",
        2 * HOUR,
        "retry_after",
        "rate_limit",
    ));
    assert_eq!(picked(&select(&mut st, &s, &at(1_000), &mut rng()).0), "c");
    st.record_cooldown(cooled(
        "c",
        "account",
        3 * HOUR,
        "retry_after",
        "rate_limit",
    ));
    assert!(matches!(
        select(&mut st, &s, &at(1_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::Timed { deadline: HOUR, ref window }) if window == "five_hour"
    ));
}

#[test]
fn a_fast_request_skips_a_credits_blocked_account_and_an_api_key_account_is_fast_eligible_whenever_eligible(
) {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1), ("c", 2)],
        kinds: vec![("c", AccountKind::ApiKey)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64| fast(request(&s, None, "claude-opus-5", true, &[], now));
    st.record_credits("a", credits(true, None, Some(0.4), 500));
    st.record_credits("b", credits(true, None, Some(0.9), 500));
    assert_eq!(
        picked(&select(&mut st, &s, &at(1_000), &mut rng()).0),
        "a",
        "credits with room"
    );
    for (blocked, why) in [
        (credits(false, None, None, 600), "disabled"),
        (
            credits(true, Some("org_level_disabled"), Some(0.1), 600),
            "naming a reason",
        ),
        (credits(true, None, Some(1.0), 600), "at the limit"),
    ] {
        let mut st = RoutingState::from_load(RoutingLoad::default());
        st.record_credits("a", blocked);
        assert_eq!(
            picked(&select(&mut st, &s, &at(1_000), &mut rng()).0),
            "b",
            "{why}"
        );
    }
    st.record_credits("a", credits(false, None, None, 600));
    st.record_credits("b", credits(true, None, Some(1.2), 600));
    assert_eq!(
        picked(&select(&mut st, &s, &at(1_000), &mut rng()).0),
        "c",
        "an API-key upstream serves a fast request whenever it serves at all"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, None, "claude-opus-5", true, &[], 1_000),
                &mut rng()
            )
            .0
        ),
        "a",
        "a standard request draws no credits"
    );
}

#[test]
fn a_fast_request_neither_releases_nor_moves_its_pin_for_a_spent_window_or_plan_overage() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64| fast(request(&s, Some("s1"), "claude-sonnet-5", true, &[], now));
    assert_eq!(picked(&select(&mut st, &s, &at(1_000), &mut rng()).0), "a");
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_overage("a", overage(true, 1_500, Some(9 * HOUR)));
    let pin = pinned(&st, "s1");
    assert_eq!(
        release_reasons(&st, &s, &pin, &at(2_000)),
        vec!["released"],
        "neither the spent window nor the overage stands against a fast request"
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &at(2_000), &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated, sel.trace.renewed),
        ("a", None, true)
    );
    let standard = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000);
    assert_eq!(
        release_reasons(&st, &s, &pin, &standard),
        vec!["overage", "spent"],
        "the same facts stand against a standard request"
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &standard, &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent))
    );

    let never = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        policy: OveragePolicy::Never,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_overage("a", overage(true, 500, Some(9 * HOUR)));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &never,
                &fast(request(&never, None, "claude-sonnet-5", true, &[], 1_000)),
                &mut rng()
            )
            .0
        ),
        "a",
        "under never, plan overage is no obstacle to a fast request"
    );
    assert_eq!(
        picked(
            &select(
                &mut st,
                &never,
                &request(&never, None, "claude-sonnet-5", true, &[], 1_000),
                &mut rng()
            )
            .0
        ),
        "b"
    );
}

#[test]
fn the_local_answer_is_fast_lane_exhausted_with_the_earliest_reset_while_one_account_is_only_rate_limited(
) {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled("a", "fast", 5_000, "fast", FAST_RATE_LIMIT_REASON));
    st.record_cooldown(cooled(
        "b",
        "fast_credits",
        3_000,
        "out_of_credits",
        FAST_CREDITS_REASON,
    ));
    st.record_cooldown(cooled(
        "c",
        "model:claude-opus-5",
        2_000,
        "retry_after",
        "rate_limit",
    ));
    let at = |now: i64| fast(request(&s, None, "claude-opus-5", true, &[], now));
    assert_eq!(
        select(&mut st, &s, &at(1_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastLane { deadline: 5_000 }),
        "the fast rate limit's own reset, however early the other rows lift"
    );
    st.record_cooldown(cooled(
        "b",
        "fast",
        4_000,
        "retry_after",
        FAST_RATE_LIMIT_REASON,
    ));
    assert_eq!(
        select(&mut st, &s, &at(1_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastLane { deadline: 5_000 }),
        "b's credits row answers ahead of the rate limit beside it, so b holds no rate limit alone"
    );
    st.clear_cooldown("b", "fast_credits");
    assert_eq!(
        select(&mut st, &s, &at(1_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastLane { deadline: 4_000 }),
        "the earliest of the fast rate limits"
    );

    // A request bound to its account answers that account's own fast row.
    let mut bound = RoutingState::from_load(RoutingLoad::default());
    let pinned_at = |now: i64| fast(request(&s, Some("s1"), "claude-opus-5", false, &[], now));
    let first = picked(&select(&mut bound, &s, &pinned_at(1_000), &mut rng()).0).to_string();
    bound.record_cooldown(cooled(
        &first,
        "fast",
        7_000,
        "fast",
        FAST_RATE_LIMIT_REASON,
    ));
    assert_eq!(
        select(&mut bound, &s, &pinned_at(2_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastLane { deadline: 7_000 })
    );
}

#[test]
fn the_local_answer_is_fast_credits_unavailable_with_the_pinned_accounts_reason_when_every_account_is_credits_blocked(
) {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64, session: Option<&str>, attempted: &[&str]| {
        fast(request(&s, session, "claude-opus-5", true, attempted, now))
    };
    assert_eq!(
        picked(&select(&mut st, &s, &at(1_000, Some("s1"), &["a"]), &mut rng()).0),
        "b"
    );
    st.record_cooldown(cooled(
        "a",
        "fast_credits",
        HOUR,
        "out_of_credits",
        FAST_CREDITS_REASON,
    ));
    st.record_credits("b", credits(false, Some("org_level_disabled"), None, 1_500));
    assert_eq!(
        select(&mut st, &s, &at(2_000, Some("s1"), &[]), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "org_level_disabled".into()
        }),
        "the pinned account's reason first"
    );
    assert_eq!(
        select(&mut st, &s, &at(2_000, None, &[]), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "out_of_credits".into()
        }),
        "without a pin, the first account's in order"
    );
    let paused = snapshot_with(
        Pool {
            members: vec![("a", 0), ("b", 1), ("c", 2)],
            ..Pool::default()
        },
        &[("c", false, AccountState::Ready)],
    );
    assert_eq!(
        select(
            &mut st,
            &paused,
            &fast(request(&paused, None, "claude-opus-5", true, &[], 2_000)),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "out_of_credits".into()
        }),
        "an account only an operator can clear leaves every account the pool could use credits-blocked"
    );
}

#[test]
fn a_fast_rate_limit_answers_its_reset_only_where_it_is_the_candidates_one_blocker() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64| fast(request(&s, None, "claude-opus-5", true, &[], now));
    st.record_cooldown(cooled("a", "fast", 5_000, "fast", FAST_RATE_LIMIT_REASON));
    st.record_credits("a", credits(true, None, Some(0.4), 500));
    assert_eq!(
        select(&mut st, &s, &at(1_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastLane { deadline: 5_000 }),
        "with credits to draw, the rate limit is the one blocker and its reset the wait"
    );
    st.record_credits("a", credits(false, Some("org_level_disabled"), None, 1_500));
    assert_eq!(
        select(&mut st, &s, &at(2_000), &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "org_level_disabled".into()
        }),
        "no wait for the fast reset clears the credits block beside it"
    );

    let budgeted = with_member_budget(&s, "a", None, Some(5.0));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled("a", "fast", 5_000, "fast", FAST_RATE_LIMIT_REASON));
    st.record_member_spend(
        &budgeted,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 50_000,
            spent_usd: 5.0,
            observed_at: 1_000,
        }],
    );
    assert_eq!(
        select(
            &mut st,
            &budgeted,
            &fast(request(&budgeted, None, "claude-opus-5", true, &[], 2_000)),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 50_000,
            window: "overage".into(),
        }),
        "nor a budget at its limit, whose period's end is the wait"
    );
}

#[test]
fn an_api_key_account_that_took_a_credits_blocked_ids_place_never_inherits_the_block() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_credits("a", credits(false, Some("out_of_credits"), None, 500));
    assert_eq!(
        select(
            &mut st,
            &s,
            &fast(request(&s, None, "claude-opus-5", true, &[], 1_000)),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::FastCredits {
            reason: "out_of_credits".into()
        })
    );
    // The OAuth holder deleted and an api_key account created under its id, before any purge has run.
    let mut load = load_of(
        Pool {
            members: vec![("a", 0)],
            kinds: vec![("a", AccountKind::ApiKey)],
            ..Pool::default()
        },
        &[],
    );
    for account in &mut load.accounts {
        account.incarnation = 2;
    }
    let replaced = RegistrySnapshot::from_load(load, [0; 32], 0, 2);
    assert_eq!(
        picked(
            &select(
                &mut st,
                &replaced,
                &fast(request(&replaced, None, "claude-opus-5", true, &[], 2_000)),
                &mut rng()
            )
            .0
        ),
        "a",
        "the previous holder's credits say nothing about the account the id now names"
    );
}

#[test]
fn a_mixed_fast_credits_and_model_cooldown_set_answers_the_timed_exhaustion_as_before() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_cooldown(cooled(
        "a",
        "fast_credits",
        5 * HOUR,
        "out_of_credits",
        FAST_CREDITS_REASON,
    ));
    st.record_cooldown(cooled(
        "b",
        "model:claude-opus-5",
        2_000,
        "retry_after",
        "rate_limit",
    ));
    assert_eq!(
        select(
            &mut st,
            &s,
            &fast(request(&s, None, "claude-opus-5", true, &[], 1_000)),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 2_000,
            window: "retry_after".into()
        })
    );
    st.record_cooldown(cooled(
        "b",
        "model:claude-opus-5",
        6 * HOUR,
        "retry_after",
        "rate_limit",
    ));
    assert_eq!(
        select(
            &mut st,
            &s,
            &fast(request(&s, None, "claude-opus-5", true, &[], 1_000)),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 5 * HOUR,
            window: "overage".into()
        }),
        "the credits row the earliest deadline claims `overage`, a window the client knows"
    );
}

#[test]
fn a_fast_pin_released_by_its_fast_lane_names_the_fast_reason_and_a_standard_one_names_cooldown() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let fast_at = |now: i64| fast(request(&s, Some("s1"), "claude-sonnet-5", true, &[], now));
    let standard_at = |now: i64| request(&s, Some("s1"), "claude-sonnet-5", true, &[], now);
    assert_eq!(
        picked(&select(&mut st, &s, &fast_at(1_000), &mut rng()).0),
        "a"
    );
    let pin = pinned(&st, "s1");
    st.record_cooldown(cooled("a", "fast", HOUR, "fast", FAST_RATE_LIMIT_REASON));
    assert_eq!(
        release_reasons(&st, &s, &pin, &fast_at(2_000)),
        vec![FAST_RATE_LIMIT_REASON]
    );
    assert_eq!(
        release_reasons(&st, &s, &pin, &standard_at(2_000)),
        vec!["released"],
        "the fast lane's row is nothing to a standard request"
    );
    st.record_cooldown(cooled("a", "account", 2 * HOUR, "five_hour", "rate_limit"));
    assert_eq!(
        release_reasons(&st, &s, &pin, &fast_at(2_000)),
        vec![FAST_RATE_LIMIT_REASON],
        "the plan window's row holds no fast request"
    );
    assert_eq!(
        release_reasons(&st, &s, &pin, &standard_at(2_000)),
        vec!["cooldown"]
    );
    st.clear_cooldown("a", "account");
    st.clear_cooldown("a", "fast");
    st.record_credits("a", credits(false, None, None, 1_500));
    assert_eq!(
        release_reasons(&st, &s, &pin, &fast_at(2_000)),
        vec![FAST_CREDITS_REASON],
        "the poll's credits state alone"
    );
    st.record_cooldown(cooled(
        "a",
        "fast_credits",
        HOUR,
        "out_of_credits",
        FAST_CREDITS_REASON,
    ));
    assert_eq!(
        release_reasons(&st, &s, &pin, &fast_at(2_000)),
        vec![FAST_CREDITS_REASON],
        "named once when the row and the state both stand"
    );
    st.record_cooldown(cooled("a", "fast", HOUR, "fast", FAST_RATE_LIMIT_REASON));
    assert_eq!(
        release_reasons(&st, &s, &pin, &fast_at(2_000)),
        vec![FAST_RATE_LIMIT_REASON, FAST_CREDITS_REASON],
        "each of the fast lane's two rows is named"
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &fast_at(2_000), &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.renewed),
        ("b", false),
        "the session is placed afresh on the sibling"
    );
}

#[test]
fn a_credits_blocked_candidate_is_skipped_under_the_credits_obstacle_in_the_trace() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_credits("a", credits(false, Some("out_of_credits"), None, 500));
    let (result, _) = select(
        &mut st,
        &s,
        &fast(request(&s, None, "claude-opus-5", true, &[], 1_000)),
        &mut rng(),
    );
    assert_eq!(picked(&result), "b");
    assert_eq!(skipped_as(&result), [("a", "credits")]);
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
fn a_marked_pin_moves_only_to_a_destination_under_the_band() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let two = |marked: &str, spare: &str| {
        loaded(vec![
            burning(marked, "a", 4.0, false, T),
            burning(spare, "a", 1.0, false, T),
        ])
    };
    let mut st = two("s1", "s0");
    st.record_sample("a", sample(0.85, 0.3, T - 500));
    st.record_sample("b", sample(0.77, 0.3, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), None),
        "0.77 is inside the band under 0.8, so b is no destination"
    );
    st.record_sample("b", sample(0.70, 0.3, T + 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed))
    );
    // A fresh state isolates the two inequalities the band applies to each window: `a` sits at exactly the
    // threshold throughout, so every call below also proves the pinned side of `>=` is inclusive.
    let mut edges = two("s2", "s9");
    edges.record_sample("a", sample(0.8, 0.1, T - 500));
    edges.record_sample("b", sample(0.1, 0.76, T - 500));
    assert_eq!(
        landed(select(&mut edges, &s, &auxiliary(&s, "s2", T), &mut rng()).0),
        ("a".into(), None),
        "b's seven-day sample sits inside the band, so the seven-day conjunct alone keeps it out"
    );
    edges.record_sample("b", sample(0.75, 0.1, T + 500));
    assert_eq!(
        landed(select(&mut edges, &s, &auxiliary(&s, "s2", T + 1_000), &mut rng()).0),
        ("a".into(), None),
        "exactly threshold minus the band is still inside it: the comparison is strict"
    );
    edges.record_sample("b", sample(0.1, 0.1, T + 1_500));
    assert_eq!(
        landed(select(&mut edges, &s, &auxiliary(&s, "s2", T + 2_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
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
    // The no-reset branch: absent a reset, a full reading stands until the next reading. Continuing s2's pin on
    // a with a no-reset spent reading: the account cannot serve, so the pin moves.
    let t = now + 1_000;
    st.record_sample("a", sample(1.0, 0.1, t));
    st.record_sample("b", refreshing(0.2, 9 * HOUR, 9 * HOUR, t));
    let SelectResult::Selected(sel) = pick2(&mut st, t + 1_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "no reset dated: a full reading stands until the next reading"
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
fn a_fable_pin_moves_off_an_account_whose_five_hour_window_is_spent_though_its_fable_week_holds() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let fable = |st: &mut RoutingState, session: &str, now: i64| {
        select(
            st,
            &s,
            &request(&s, Some(session), "claude-fable-5-1", true, &[], now),
            &mut rng(),
        )
    };
    assert_eq!(picked(&fable(&mut st, "s1", 1_000).0), "a");
    st.record_sample(
        "a",
        with_fable(
            refreshing(1.0, 2 * HOUR, 9 * HOUR, 1_500),
            0.5,
            Some(9 * HOUR),
        ),
    );
    st.record_sample(
        "b",
        with_fable(sample(0.2, 0.1, 1_500), 0.5, Some(9 * HOUR)),
    );
    let SelectResult::Selected(sel) = fable(&mut st, "s1", 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "a Fable request draws on the shared five-hour window too, so a spent one moves the pin"
    );

    let only_a = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    assert_eq!(
        select(
            &mut st,
            &only_a,
            &request(&only_a, Some("s2"), "claude-fable-5-1", true, &[], 3_000),
            &mut rng()
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 2 * HOUR,
            window: FIVE_HOUR.into()
        }),
        "a fresh Fable placement passes the account over, dated by the window that is spent"
    );
}

#[test]
fn under_allow_a_spent_model_week_places_behind_a_plan_funded_sibling_while_extra_usage_is_enabled()
{
    for prefer_subscription in [false, true] {
        let s = snapshot(Pool {
            policy: OveragePolicy::Allow,
            members: vec![("a", 0), ("b", 1)],
            prefer_subscription,
            ..Pool::default()
        });
        let mut st = RoutingState::from_load(RoutingLoad::default());
        st.record_sample(
            "a",
            with_fable(sample(0.2, 0.1, 1_000), 1.0, Some(9 * HOUR)),
        );
        st.record_sample(
            "b",
            with_fable(sample(0.3, 0.1, 1_000), 0.5, Some(9 * HOUR)),
        );
        st.record_credits("a", credits(true, None, Some(0.2), 1_000));
        let fable = |st: &mut RoutingState, now: i64| {
            select(
                st,
                &s,
                &request(&s, None, "claude-fable-5-1", true, &[], now),
                &mut rng(),
            )
            .0
        };
        assert_eq!(
            picked(&fable(&mut st, 2_000)),
            "b",
            "a plan-funded sibling goes first whatever its priority, kind order {prefer_subscription}"
        );
        st.record_cooldown(cooled(
            "b",
            "account",
            2_000 + HOUR,
            "five_hour",
            "rate_limit",
        ));
        assert_eq!(
            picked(&fable(&mut st, 3_000)),
            "a",
            "with the sibling cooled, the spent week goes on in extra usage, kind order {prefer_subscription}"
        );
        st.record_credits("a", credits(false, None, None, 4_000));
        assert!(
            matches!(fable(&mut st, 5_000), SelectResult::Exhausted(_)),
            "extra usage disabled funds nothing, kind order {prefer_subscription}"
        );
        let mut unread = RoutingState::from_load(RoutingLoad::default());
        unread.record_sample(
            "a",
            with_fable(sample(0.2, 0.1, 1_000), 1.0, Some(9 * HOUR)),
        );
        unread.record_cooldown(cooled(
            "b",
            "account",
            2_000 + HOUR,
            "five_hour",
            "rate_limit",
        ));
        assert!(
            matches!(fable(&mut unread, 3_000), SelectResult::Exhausted(_)),
            "a credits state the poll has not read is not extra usage enabled, kind order {prefer_subscription}"
        );
    }
}

#[test]
fn under_never_a_spent_model_week_drops_its_account_whatever_the_credits_read() {
    let s = snapshot(Pool {
        policy: OveragePolicy::Never,
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample(
        "a",
        with_fable(sample(0.2, 0.1, 1_000), 1.0, Some(9 * HOUR)),
    );
    st.record_sample(
        "b",
        with_fable(sample(0.3, 0.1, 1_000), 0.5, Some(9 * HOUR)),
    );
    st.record_credits("a", credits(true, None, Some(0.2), 1_000));
    let fable = |st: &mut RoutingState, now: i64| {
        select(
            st,
            &s,
            &request(&s, None, "claude-fable-5-1", true, &[], now),
            &mut rng(),
        )
        .0
    };
    let result = fable(&mut st, 2_000);
    assert_eq!(picked(&result), "b");
    assert!(
        skipped_as(&result).contains(&("a", "spent")),
        "{:?}",
        skipped_as(&result)
    );
    st.record_cooldown(cooled(
        "b",
        "account",
        2_000 + HOUR,
        "five_hour",
        "rate_limit",
    ));
    assert!(
        matches!(fable(&mut st, 3_000), SelectResult::Exhausted(_)),
        "never continues no spent week in extra usage"
    );
}

#[test]
fn a_seven_day_opus_reading_at_its_limit_is_kept_in_the_sample_and_drops_nothing() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let window = |key: &str, utilization: f64| phoebis_oauth::UsageWindow {
        key: key.into(),
        utilization: Some(utilization),
        resets_at: Some(9 * HOUR),
    };
    let (sample, _) = phoebis_routing::sample_from_usage(
        &phoebis_oauth::UsageSnapshot {
            account_id: "a".into(),
            identity_generation: 1,
            fetched_at: 1_000,
            windows: vec![
                window("five_hour", 20.0),
                window("seven_day", 10.0),
                window("seven_day_opus", 100.0),
            ],
            fable_window: None,
            extra_usage: None,
            raw_keys: Vec::new(),
            error: None,
        },
        1,
    );
    st.record_sample("a", sample);
    assert_eq!(
        st.sample("a")
            .and_then(|s| s.reading("seven_day_opus"))
            .map(|r| r.utilization),
        Some(1.0),
        "a week the family table has no row for is kept under its own key"
    );
    for model in ["claude-opus-5", "claude-fable-5-1", "claude-sonnet-5"] {
        assert_eq!(
            picked(
                &select(
                    &mut st,
                    &s,
                    &request(&s, None, model, true, &[], 2_000),
                    &mut rng()
                )
                .0
            ),
            "a",
            "no row reads the week, so it feeds no allowance for {model}"
        );
    }
}

#[test]
fn an_unknown_seven_day_reading_drops_nothing() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample(
        "a",
        sample(0.2, 0.1, 1_000).with(
            "seven_day_x",
            WindowReading {
                utilization: 1.0,
                reset: Some(9 * HOUR),
                observed_at: 1_000,
            },
        ),
    );
    for model in ["claude-fable-5-1", "claude-sonnet-5"] {
        assert_eq!(
            picked(
                &select(
                    &mut st,
                    &s,
                    &request(&s, None, model, true, &[], 2_000),
                    &mut rng()
                )
                .0
            ),
            "a",
            "{model}"
        );
    }
}

#[test]
fn a_spent_window_moves_a_pin_back_to_the_account_that_shed_it_inside_the_dwell() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = loaded(vec![
        burning("s1", "a", 4.0, false, T),
        burning("s0", "a", 1.0, false, T),
    ]);
    st.record_sample("a", refreshing(0.85, T + 9 * HOUR, T + 9 * HOUR, T - 500));
    st.record_sample("b", refreshing(0.3, T + 9 * HOUR, T + 9 * HOUR, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed))
    );
    // Inside the dwell, b's window is spent and a has recovered. a shed the pin and ran a round of its own, so
    // no round sends the pin back; the spent move reads neither.
    st.record_sample("b", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    st.record_sample("a", refreshing(0.2, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    let (result, writes) = select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng());
    assert_eq!(landed(result), ("a".into(), Some(MigrateReason::Spent)));
    assert!(writes.iter().any(|w| matches!(w, StateWrite::DeletePin(_))));
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
    let pin = st.pin(&pin_key("s1")).cloned().unwrap();
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
fn a_member_at_its_budget_is_skipped_until_the_periods_end_and_serves_again_after_it() {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        }),
        "a",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_member_spend(
        &s,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 10_000,
            spent_usd: 5.0,
            observed_at: 1_000,
        }],
    );
    let SelectResult::Selected(sel) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    )
    .0
    else {
        panic!("a sibling serves the request")
    };
    assert_eq!(
        sel.account.id, "b",
        "a is at its budget, so the sibling takes the request"
    );
    assert!(
        sel.trace
            .skipped
            .iter()
            .any(|sk| sk.account_id == "a" && sk.obstacle == "budget"),
        "{:?}",
        sel.trace.skipped
    );
    let SelectResult::Selected(sel) = select(
        &mut st,
        &s,
        &request(&s, Some("s2"), "claude-sonnet-5", true, &[], 10_000),
        &mut rng(),
    )
    .0
    else {
        panic!("a serves once the period has ended")
    };
    assert_eq!(sel.account.id, "a", "the period ended, so a serves again");
    st.record_member_spend(
        &s,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 10_000,
            period_end: 20_000,
            spent_usd: 4.0,
            observed_at: 15_000,
        }],
    );
    let SelectResult::Selected(sel) = select(
        &mut st,
        &s,
        &request(&s, Some("s3"), "claude-sonnet-5", true, &[], 15_000),
        &mut rng(),
    )
    .0
    else {
        panic!("a serves under its budget")
    };
    assert_eq!(
        sel.account.id, "a",
        "a report below the budget never excludes"
    );
}

/// A spend row of `a` summed over `period` from 0 to `period_end`, as the ledger would report it at 1,000.
fn spend_of_a(period: BudgetPeriod, period_end: i64, spent_usd: f64) -> MemberSpend {
    MemberSpend {
        pool_id: "p".into(),
        account_id: "a".into(),
        period,
        period_start: 0,
        period_end,
        spent_usd,
        observed_at: 1_000,
    }
}

#[test]
fn a_member_with_both_budgets_is_skipped_when_either_is_reached() {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        }),
        "a",
        Some(5.0),
        Some(50.0),
    );
    let pick = |st: &mut RoutingState, session: &str| {
        let SelectResult::Selected(sel) = select(
            st,
            &s,
            &request(&s, Some(session), "claude-sonnet-5", true, &[], 1_000),
            &mut rng(),
        )
        .0
        else {
            panic!("a member serves the request")
        };
        let skipped = sel
            .trace
            .skipped
            .iter()
            .any(|sk| sk.account_id == "a" && sk.obstacle == "budget");
        (sel.account.id.to_string(), skipped)
    };
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_member_spend(
        &s,
        vec![
            spend_of_a(BudgetPeriod::Day, 10_000, 1.0),
            spend_of_a(BudgetPeriod::Month, 100_000, 25.0),
        ],
    );
    assert_eq!(
        pick(&mut st, "s1"),
        ("a".to_string(), false),
        "under both budgets, a serves"
    );
    st.record_member_spend(
        &s,
        vec![
            spend_of_a(BudgetPeriod::Day, 10_000, 5.0),
            spend_of_a(BudgetPeriod::Month, 100_000, 25.0),
        ],
    );
    assert_eq!(
        pick(&mut st, "s2"),
        ("b".to_string(), true),
        "the day's budget is reached, so a is skipped though the month's has room"
    );
    st.record_member_spend(
        &s,
        vec![
            spend_of_a(BudgetPeriod::Day, 10_000, 1.0),
            spend_of_a(BudgetPeriod::Month, 100_000, 50.0),
        ],
    );
    assert_eq!(
        pick(&mut st, "s3"),
        ("b".to_string(), true),
        "the month's budget is reached, so a is skipped though the day's has room"
    );
}

#[test]
fn a_member_whose_monthly_budget_is_reached_answers_the_months_end_not_the_days() {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0)],
            ..Pool::default()
        }),
        "a",
        Some(5.0),
        Some(50.0),
    );
    let answer = |st: &mut RoutingState| {
        select(
            st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
            &mut rng(),
        )
        .0
    };
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_member_spend(
        &s,
        vec![
            spend_of_a(BudgetPeriod::Day, 10_000, 5.0),
            spend_of_a(BudgetPeriod::Month, 100_000, 50.0),
        ],
    );
    assert_eq!(
        answer(&mut st),
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 100_000,
            window: "overage".into(),
        }),
        "the day's end clears nothing while the month's budget stands, so the month's end is the wait"
    );
    st.record_member_spend(
        &s,
        vec![
            spend_of_a(BudgetPeriod::Day, 10_000, 5.0),
            spend_of_a(BudgetPeriod::Month, 100_000, 25.0),
        ],
    );
    assert_eq!(
        answer(&mut st),
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 10_000,
            window: "overage".into(),
        }),
        "only the day's budget is reached, so the day's end is the wait"
    );
}

#[test]
fn every_member_at_its_budget_answers_a_timed_exhaustion_under_the_overage_window() {
    let s = with_member_budget(
        &with_member_budget(
            &snapshot(Pool {
                members: vec![("a", 0), ("b", 1)],
                ..Pool::default()
            }),
            "a",
            None,
            Some(5.0),
        ),
        "b",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_member_spend(
        &s,
        vec![
            MemberSpend {
                pool_id: "p".into(),
                account_id: "a".into(),
                period: BudgetPeriod::Month,
                period_start: 0,
                period_end: 10_000,
                spent_usd: 5.0,
                observed_at: 1_000,
            },
            MemberSpend {
                pool_id: "p".into(),
                account_id: "b".into(),
                period: BudgetPeriod::Month,
                period_start: 0,
                period_end: 8_000,
                spent_usd: 5.0,
                observed_at: 1_000,
            },
        ],
    );
    assert_eq!(
        select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
            &mut rng(),
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 8_000,
            window: "overage".into(),
        }),
        "every member is at its budget, so the pool answers the earlier period's end"
    );
}

#[test]
fn a_pin_on_a_member_at_its_budget_is_released_with_reason_budget() {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        }),
        "a",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "a serves before its spend reaches the budget"
    );
    let pin = st
        .pin(&pin_key("s1"))
        .cloned()
        .expect("the first select pinned a");
    st.record_member_spend(
        &s,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 10_000,
            spent_usd: 5.0,
            observed_at: 2_000,
        }],
    );
    let at_budget = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000);
    assert!(
        release_reasons(&st, &s, &pin, &at_budget).contains(&"budget"),
        "the member's spend reached its budget, so the pin releases as budget"
    );
}

#[test]
fn a_probe_on_a_pin_whose_member_is_at_its_budget_answers_the_periods_end_and_moves_nothing() {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        }),
        "a",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "a serves before its spend reaches the budget"
    );
    st.record_member_spend(
        &s,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 10_000,
            spent_usd: 5.0,
            observed_at: 2_000,
        }],
    );
    let probe = SelectRequest {
        probe: true,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000)
    };
    let (result, writes) = select(&mut st, &s, &probe, &mut rng());
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 10_000,
            window: "budget".into(),
        }),
        "the probe answers the budget's own deadline rather than moving to a sibling"
    );
    assert_eq!(
        st.pin(&pin_key("s1")).map(|p| p.account_id.as_str()),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
    let SelectResult::Selected(sel) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000),
        &mut rng(),
    )
    .0
    else {
        panic!("a real request still moves the pin to the sibling")
    };
    assert_eq!(
        sel.account.id, "b",
        "a real request still moves the pin off the member at its budget"
    );
}

/// Members `a` (priority 0, a monthly budget) and `b` (priority 1, none), and a state whose session `s1` is pinned to
/// `a` by a first selection while the ledger was current.
fn pinned_to_a_budgeted_member() -> (RegistrySnapshot, RoutingState) {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        }),
        "a",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "a serves while the ledger is current"
    );
    (s, st)
}

#[test]
fn a_degraded_ledger_moves_a_budgeted_members_session_to_its_unbudgeted_sibling() {
    let (s, mut st) = pinned_to_a_budgeted_member();
    let pin = st
        .pin(&pin_key("s1"))
        .cloned()
        .expect("the first select pinned a");
    st.set_accounting_degraded(true);
    let degraded = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000);
    assert_eq!(
        release_reasons(&st, &s, &pin, &degraded),
        ["budget_unknown"],
        "the member's spend may be stale, so the pin releases under that reason alone"
    );
    let result = select(&mut st, &s, &degraded, &mut rng()).0;
    assert_eq!(
        picked(&result),
        "b",
        "the session moves to the sibling without a budget"
    );
    assert_eq!(skipped_as(&result), [("a", "budget_unknown")]);
}

#[test]
fn a_pool_of_budgeted_members_alone_answers_untimed_while_the_ledger_is_degraded() {
    let s = with_member_budget(
        &with_member_budget(
            &snapshot(Pool {
                members: vec![("a", 0), ("b", 1)],
                ..Pool::default()
            }),
            "a",
            None,
            Some(5.0),
        ),
        "b",
        Some(5.0),
        None,
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_member_spend(
        &s,
        vec![
            MemberSpend {
                pool_id: "p".into(),
                account_id: "a".into(),
                period: BudgetPeriod::Month,
                period_start: 0,
                period_end: 10_000,
                spent_usd: 0.0,
                observed_at: 1_000,
            },
            MemberSpend {
                pool_id: "p".into(),
                account_id: "b".into(),
                period: BudgetPeriod::Day,
                period_start: 0,
                period_end: 8_000,
                spent_usd: 0.0,
                observed_at: 1_000,
            },
        ],
    );
    st.set_accounting_degraded(true);
    assert_eq!(
        select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
            &mut rng(),
        )
        .0,
        SelectResult::Exhausted(Exhaustion::Untimed),
        "whatever the spend rows say, no budgeted member is placed on and no deadline dates the answer"
    );
}

#[test]
fn a_probe_on_a_pin_whose_member_spend_is_unknown_answers_the_obstacle_and_moves_nothing() {
    let (s, mut st) = pinned_to_a_budgeted_member();
    st.set_accounting_degraded(true);
    let probe = SelectRequest {
        probe: true,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000)
    };
    let (result, writes) = select(&mut st, &s, &probe, &mut rng());
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Pinned {
            obstacle: "budget_unknown",
        }),
        "the probe answers the untimed obstacle rather than a budget's deadline or a sibling"
    );
    assert_eq!(
        st.pin(&pin_key("s1")).map(|p| p.account_id.as_str()),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(writes.is_empty(), "the probe moves no pin: {writes:?}");
}

/// A pin on `a` written by a request under `before`, then a probe of that session under `at_probe` at a later
/// instant, `attempted` naming the accounts the probe already tried: the probe's answer, its writes, and the
/// account the session's pin names afterwards.
fn probe_after_a_pin_on_a(
    before: &RegistrySnapshot,
    at_probe: &RegistrySnapshot,
    attempted: &[&str],
) -> (SelectResult, Vec<StateWrite>, Option<String>) {
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                before,
                &request(before, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "a serves the session's first request"
    );
    let probe = SelectRequest {
        probe: true,
        ..request(
            at_probe,
            Some("s1"),
            "claude-sonnet-5",
            true,
            attempted,
            2_000,
        )
    };
    let (result, writes) = select(&mut st, at_probe, &probe, &mut rng());
    let pinned = st.pin(&pin_key("s1")).map(|p| p.account_id.clone());
    (result, writes, pinned)
}

fn a_before_b() -> Pool {
    Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    }
}

#[test]
fn a_probe_on_a_pin_whose_account_the_operator_paused_answers_the_obstacle_and_moves_nothing() {
    let paused = snapshot_with(a_before_b(), &[("a", false, AccountState::Ready)]);
    let (result, writes, pinned) = probe_after_a_pin_on_a(&snapshot(a_before_b()), &paused, &[]);
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Pinned {
            obstacle: "operator"
        }),
        "the probe answers the pause rather than moving to a sibling"
    );
    assert_eq!(
        pinned.as_deref(),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
}

#[test]
fn a_probe_on_a_pin_whose_subscription_ended_answers_the_obstacle_and_moves_nothing() {
    let ended = snapshot_with_terms(
        a_before_b(),
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(1_500),
            tier: None,
        }],
    );
    let (result, writes, pinned) = probe_after_a_pin_on_a(&snapshot(a_before_b()), &ended, &[]);
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Pinned {
            obstacle: "subscription_ended"
        }),
        "the probe answers the ended subscription rather than moving to a sibling"
    );
    assert_eq!(
        pinned.as_deref(),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
}

#[test]
fn a_probe_that_already_tried_its_pinned_account_answers_attempted_and_moves_nothing() {
    let s = snapshot(a_before_b());
    let (result, writes, pinned) = probe_after_a_pin_on_a(&s, &s, &["a"]);
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Pinned {
            obstacle: "attempted"
        }),
        "the probe answers its own attempt rather than moving to a sibling"
    );
    assert_eq!(
        pinned.as_deref(),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
}

/// The probe's own attempt marked `a` for re-authentication, and the mark reached the snapshot before the probe's
/// reselection: `a` reads as the operator's obstacle, yet it is still the member this probe tried.
#[test]
fn a_probe_whose_attempt_marked_its_pinned_account_answers_attempted_and_moves_nothing() {
    let marked = snapshot_with(a_before_b(), &[("a", true, AccountState::ReauthRequired)]);
    let (result, writes, pinned) = probe_after_a_pin_on_a(&snapshot(a_before_b()), &marked, &["a"]);
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Pinned {
            obstacle: "attempted"
        }),
        "the probe answers its own attempt rather than moving to the free sibling"
    );
    assert_eq!(
        pinned.as_deref(),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
}

#[test]
fn a_probe_that_tried_its_pinned_account_with_no_sibling_left_answers_the_pools_exhaustion() {
    let s = snapshot(a_before_b());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "a serves the session's first request"
    );
    st.record_cooldown(cooled("b", "account", 10_000, "five_hour", "rate_limit"));
    let probe = SelectRequest {
        probe: true,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &["a"], 2_000)
    };
    let (result, writes) = select(&mut st, &s, &probe, &mut rng());
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 10_000,
            window: "five_hour".into(),
        }),
        "with no sibling to move to, the attempt hides nothing and the pool's own exhaustion answers"
    );
    assert_eq!(
        st.pin(&pin_key("s1")).map(|p| p.account_id.as_str()),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
}

/// The probe's own attempt marked `a` and the mark reached the snapshot, while `b` is cooling: with no sibling
/// to move to, the attempt hides nothing, so the pool's exhaustion answers as it does for an unmarked attempt.
#[test]
fn a_probe_whose_attempt_marked_its_pinned_account_with_no_sibling_left_answers_the_pools_exhaustion(
) {
    let s = snapshot(a_before_b());
    let marked = snapshot_with(a_before_b(), &[("a", true, AccountState::ReauthRequired)]);
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a",
        "a serves the session's first request"
    );
    st.record_cooldown(cooled("b", "account", 10_000, "five_hour", "rate_limit"));
    let probe = SelectRequest {
        probe: true,
        ..request(&marked, Some("s1"), "claude-sonnet-5", true, &["a"], 2_000)
    };
    let (result, writes) = select(&mut st, &marked, &probe, &mut rng());
    assert_eq!(
        result,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 10_000,
            window: "five_hour".into(),
        }),
        "with no sibling to move to, the marked member's attempt answers the pool's exhaustion"
    );
    assert_eq!(
        st.pin(&pin_key("s1")).map(|p| p.account_id.as_str()),
        Some("a"),
        "the pin still names a after the probe"
    );
    assert!(
        writes.is_empty(),
        "the probe moves no pin and releases nothing: {writes:?}"
    );
}

#[test]
fn a_non_portable_request_bound_to_a_member_at_its_budget_answers_the_timed_exhaustion_with_the_overage_claim(
) {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        }),
        "a",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    st.record_member_spend(
        &s,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 10_000,
            spent_usd: 5.0,
            observed_at: 2_000,
        }],
    );
    let bound = request(&s, Some("s1"), "claude-sonnet-5", false, &[], 2_000);
    assert_eq!(
        select(&mut st, &s, &bound, &mut rng()).0,
        SelectResult::Exhausted(Exhaustion::Timed {
            deadline: 10_000,
            window: "overage".into(),
        }),
        "bound to a member at its budget, the request answers the timed overage claim rather than trying b"
    );
}

#[test]
fn a_spend_row_of_another_period_than_the_members_is_no_obstacle() {
    let pool = || Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    };
    // Recorded under the Month-configured snapshot, so `record_member_spend` keeps the row; the Day-configured
    // snapshot below is what the selector's own period check reads it against.
    let s_month = with_member_budget(&snapshot(pool()), "a", None, Some(5.0));
    let s_day = with_member_budget(&snapshot(pool()), "a", Some(5.0), None);
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_member_spend(
        &s_month,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 100_000,
            spent_usd: 5.0,
            observed_at: 1_000,
        }],
    );
    let pick = |st: &mut RoutingState, session: &str| {
        let result = select(
            st,
            &s_day,
            &request(&s_day, Some(session), "claude-sonnet-5", true, &[], 1_000),
            &mut rng(),
        )
        .0;
        picked(&result).to_string()
    };
    assert_eq!(
        pick(&mut st, "s1"),
        "a",
        "a Month row is no obstacle for a member configured on Day"
    );

    st.record_member_spend(
        &s_day,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Day,
            period_start: -48 * HOUR,
            period_end: -24 * HOUR,
            spent_usd: 5.0,
            observed_at: 1_000,
        }],
    );
    assert_eq!(
        pick(&mut st, "s2"),
        "a",
        "a Day row whose bounds are yesterday's is no obstacle"
    );

    st.record_member_spend(
        &s_day,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Day,
            period_start: 24 * HOUR,
            period_end: 48 * HOUR,
            spent_usd: 5.0,
            observed_at: 1_000,
        }],
    );
    assert_eq!(
        pick(&mut st, "s3"),
        "a",
        "a Day row whose bounds have not started yet is no obstacle"
    );
}

#[test]
fn a_tail_match_places_on_the_entrys_account_inside_its_ttl_and_the_strategy_places_past_it() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "a",
            1_000,
            5_000,
            800,
            "root",
        )],
    );
    let req = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["m1"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("a tail match places the request")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 1,
            prefix_tokens: 800,
        }),
    );
    let past_ttl = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 7_000),
        &["m1"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &past_ttl, &mut rng()).0 else {
        panic!("the strategy places past the ttl")
    };
    assert_eq!(
        sel.trace.placement, None,
        "the entry is no longer live, so the strategy chose instead"
    );
}

#[test]
fn a_static_match_places_a_subagent_under_by_cohort_and_a_main_conversation_only_under_by_warm_prefix(
) {
    let by_cohort = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &by_cohort,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let sub_req = subagent(
        placement_req(
            request(
                &by_cohort,
                Some("session"),
                "claude-sonnet-5",
                true,
                &[],
                2_000,
            ),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &by_cohort, &sub_req, &mut rng()).0 else {
        panic!("a subagent places under by_cohort")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "static",
            depth: 1,
            prefix_tokens: 20_000,
        }),
    );

    let by_strategy_main = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st2,
        vec![affinity_row(
            &by_strategy_main,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let main_req = placement_req(
        request(
            &by_strategy_main,
            Some("session"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &[],
        &["s1"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st2, &by_strategy_main, &main_req, &mut rng()).0
    else {
        panic!("a main conversation still lands somewhere")
    };
    assert_eq!(
        sel.trace.placement, None,
        "session_placement defaults to by_strategy, so a main conversation ignores the static tier"
    );

    let by_warm_prefix = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        session_placement: SessionPlacement::ByWarmPrefix,
        ..Pool::default()
    });
    let mut st3 = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st3,
        vec![affinity_row(
            &by_warm_prefix,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let main_req2 = placement_req(
        request(
            &by_warm_prefix,
            Some("session"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &[],
        &["s1"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st3, &by_warm_prefix, &main_req2, &mut rng()).0
    else {
        panic!("a main conversation places under by_warm_prefix")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "static",
            depth: 1,
            prefix_tokens: 20_000,
        }),
    );
}

#[test]
fn an_entry_another_key_wrote_places_the_request() {
    let s = snapshot(Pool {
        // b outranks a, so only placement (not the strategy) can land the request on a.
        members: vec![("a", 1), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut row = affinity_row(
        &s,
        "claude-sonnet-5",
        "m1",
        AffinityChain::Message,
        "a",
        1_000,
        300_000,
        900,
        "root",
    );
    row.key_id = "other".into();
    row.key_generation = 0;
    seed_affinity(&st, vec![row]);
    let req = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_500),
        &["m1"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("an entry from another key still places the request")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 1,
            prefix_tokens: 900,
        }),
    );
    assert_eq!(
        pinned(&st, "s1").key_id,
        "k",
        "the pin is made under the request's own key, not the entry's"
    );
}

#[test]
fn two_pools_sharing_an_account_share_its_entries() {
    let account = |id: &str| AccountRow {
        id: id.into(),
        kind: AccountKind::Oauth,
        label: id.into(),
        enabled: true,
        identity: None,
        lifecycle_generation: 1,
        incarnation: 1,
        revision: 1,
        state: AccountState::Ready,
    };
    let key = |id: &str, pool_id: &str| ApiKeyRow {
        id: id.into(),
        label: id.into(),
        notes: None,
        pool_id: pool_id.into(),
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
        auto_mode_server: None,
    };
    let pool = |id: &str| PoolRow {
        id: id.into(),
        label: id.into(),
        enabled: true,
        strategy: Strategy::Priority,
        affinity_ttl_seconds: 3_600,
        overage_policy: OveragePolicy::PreferPlan,
        migrate_above_utilization: None,
        select_below_utilization: None,
        model_rules: Vec::new(),
        keepalive_json: "{}".into(),
        body_capture_hours: 0,
        prefer_subscription: false,
        shedding: Shedding::default(),
        failover: Failover::default(),
        cache_ttl_json: "{}".into(),
        cache_diagnostics: false,
        auto_mode_server: true,
        affinity: Affinity::default(),
        created_at: 0,
        updated_at: 0,
    };
    let member = |pool_id: &str, account_id: &str, priority: u32| PoolMember {
        pool_id: pool_id.into(),
        account_id: account_id.into(),
        priority,
        budget_daily_usd: None,
        budget_monthly_usd: None,
    };
    let load = RegistryLoad {
        accounts: vec![account("a"), account("b"), account("c")],
        credential_generations: Vec::new(),
        keys: vec![key("k1", "p1"), key("k2", "p2")],
        // The account under test outranks neither sibling, so only placement (not the strategy) can land the
        // request on it.
        members: vec![
            member("p1", "a", 1),
            member("p1", "b", 0),
            member("p2", "a", 1),
            member("p2", "c", 0),
        ],
        pools: vec![pool("p1"), pool("p2")],
        retired_secrets: Vec::new(),
        reused_key_ids: Vec::new(),
        terms: Vec::new(),
    };
    let s = RegistrySnapshot::from_load(load, [0; 32], 0, 1);
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let row = AffinityRow {
        model: "claude-sonnet-5".into(),
        hash: "m1".into(),
        chain: AffinityChain::Message,
        account_id: "a".into(),
        incarnation: 1,
        written_at: 1_000,
        ttl_ms: 300_000,
        prefix_tokens: 900,
        key_id: "k1".into(),
        key_generation: s.key("k1").unwrap().created_at,
        root: "root".into(),
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
    };
    seed_affinity(&st, vec![row]);
    let req_for = |key_id: &str, session: &str, now: i64| SelectRequest {
        key_id: key_id.into(),
        key_generation: s.key(key_id).unwrap().created_at,
        agent_id: "main".into(),
        session_id: Some(session.into()),
        canonical_model: "claude-sonnet-5".into(),
        candidates: resolve(
            &s,
            s.key(key_id).unwrap(),
            "claude-sonnet-5",
            "claude-sonnet-5",
        )
        .unwrap(),
        portable: true,
        attempted: Vec::new(),
        now,
        fallback_ms: 60_000,
        revision: s.revision,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        static_hash: None,
        boundaries: vec!["m1".into()],
        client: ClientClass::ClaudeCode,
        static_boundaries: Vec::new(),
        marker: false,
        parent_agent_id: None,
        holds: Vec::new(),
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        fast: false,
    };
    let SelectResult::Selected(sel1) =
        select(&mut st, &s, &req_for("k1", "s1", 1_500), &mut rng()).0
    else {
        panic!("pool p1's request lands on the shared entry")
    };
    assert_eq!(
        sel1.account.id, "a",
        "pool p1's request lands on a via the shared entry"
    );
    assert_eq!(
        sel1.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 1,
            prefix_tokens: 900,
        }),
    );
    let SelectResult::Selected(sel2) =
        select(&mut st, &s, &req_for("k2", "s2", 1_500), &mut rng()).0
    else {
        panic!("pool p2's request lands on the shared entry")
    };
    assert_eq!(
        sel2.account.id, "a",
        "pool p2's request lands on the same entry"
    );
    assert_eq!(
        sel2.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 1,
            prefix_tokens: 900,
        }),
    );
}

#[test]
fn the_blocker_refuses_a_hot_account_and_the_request_lands_by_the_strategy() {
    let scenario = |pool: Pool, seed: &dyn Fn(&mut RoutingState)| {
        let s = snapshot(pool);
        let mut st = RoutingState::from_load(RoutingLoad::default());
        seed_affinity(
            &st,
            vec![affinity_row(
                &s,
                "claude-sonnet-5",
                "s1",
                AffinityChain::Static,
                "a",
                1_000,
                3_600_000,
                20_000,
                "root",
            )],
        );
        seed(&mut st);
        let req = subagent(
            placement_req(
                request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
                &[],
                &["s1"],
                true,
            ),
            "worker",
        );
        let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
            panic!("the strategy still lands the request")
        };
        assert_eq!(
            sel.account.id, "b",
            "b takes it since a is refused by the blocker"
        );
        assert_eq!(sel.trace.placement, None);
    };
    // At or above `migrate_above_utilization` minus the band on the 5-hour window.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            migrate: Some(0.8),
            ..Pool::default()
        },
        &|st| {
            st.record_sample("a", sample(0.8, 0.0, 1_000));
        },
    );
    // At or above the ceiling.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            select_below: Some(0.5),
            ..Pool::default()
        },
        &|st| {
            st.record_sample("a", sample(0.5, 0.0, 1_000));
        },
    );
    // At or under the Pace floor: `window_for` reads the seven-day window for this model.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            strategy: Strategy::Pace,
            ..Pool::default()
        },
        &|st| {
            st.record_sample("a", sample(0.0, 0.96, 1_000));
        },
    );
    // Spent: no allowance left in the request's window.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            ..Pool::default()
        },
        &|st| {
            st.record_sample("a", sample(1.0, 1.0, 1_000));
        },
    );
    // Cooling: a cooldown makes the account ineligible, so it never joins the candidates placement chooses among.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            ..Pool::default()
        },
        &|st| {
            st.record_cooldown(cooled("a", "account", 10_000, "five_hour", "rate_limit"));
        },
    );
}

#[test]
fn the_blocker_refuses_a_winding_down_account_for_the_static_tier() {
    let s = snapshot_with_terms(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            failover: Failover {
                wind_down_minutes: 30,
                ..Failover::default()
            },
            ..Pool::default()
        },
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(1_000 + 10 * 60_000), // 10 minutes out, inside the 30-minute window
            tier: None,
        }],
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let req = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!(
            "the strategy lands the request since the static tier refuses the winding-down account"
        )
    };
    assert_eq!(sel.account.id, "b", "b takes it since a is winding down");
    assert_eq!(sel.trace.placement, None);
}

#[test]
fn the_blocker_reads_each_candidate_itself_when_narrow_kept_the_wider_set() {
    let s = snapshot(Pool {
        members: vec![("a", 1), ("b", 0)],
        select_below: Some(0.3),
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", sample(0.9, 0.0, 1_000));
    st.record_sample("b", sample(0.9, 0.0, 1_000));
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let req = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("the strategy lands the request")
    };
    assert_eq!(
        sel.account.id, "b",
        "the strategy lands it since a's own reading refuses rung 4 even though narrow kept the wider set"
    );
    assert_eq!(sel.trace.placement, None);
    assert!(
        !sel.trace
            .skipped
            .iter()
            .any(|sk| sk.account_id == "a" && sk.obstacle == "above_ceiling"),
        "narrow left both accounts in the set: a is not skipped as above_ceiling, {:?}",
        sel.trace.skipped
    );
}

#[test]
fn the_size_gate_refuses_a_small_prefix_for_a_main_conversation_and_not_for_a_subagent() {
    let s = snapshot(Pool {
        members: vec![("a", 1), ("b", 0)],
        session_placement: SessionPlacement::ByWarmPrefix,
        min_shared_prefix_tokens: 20_000,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            500,
            "root",
        )],
    );
    let main_req = placement_req(
        request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
        &[],
        &["s1"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &main_req, &mut rng()).0 else {
        panic!("b lands the main conversation")
    };
    assert_eq!(
        sel.trace.placement, None,
        "a small prefix never places a main conversation"
    );
    assert_eq!(sel.account.id, "b");

    let sub_req = subagent(
        placement_req(
            request(&s, Some("session2"), "claude-sonnet-5", true, &[], 2_000),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &sub_req, &mut rng()).0 else {
        panic!("a places the subagent")
    };
    assert_eq!(sel.account.id, "a", "any positive prefix places a subagent");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "static",
            depth: 1,
            prefix_tokens: 500,
        }),
    );
}

#[test]
fn two_large_cached_requests_sharing_only_the_absent_tools_boundary_never_place_on_each_other() {
    let by_cohort = snapshot(Pool {
        members: vec![("a", 1), ("b", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &by_cohort,
            "claude-sonnet-5",
            "t0",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            0,
            "root",
        )],
    );
    let sub_req = subagent(
        placement_req(
            request(
                &by_cohort,
                Some("session"),
                "claude-sonnet-5",
                true,
                &[],
                2_000,
            ),
            &[],
            &["t0"],
            true,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &by_cohort, &sub_req, &mut rng()).0 else {
        panic!("b lands the subagent")
    };
    assert_eq!(
        sel.trace.placement, None,
        "the shared boundary carries no promised prefix, so a subagent does not place on it"
    );
    assert_eq!(sel.account.id, "b");

    let by_warm_prefix = snapshot(Pool {
        members: vec![("a", 1), ("b", 0)],
        session_placement: SessionPlacement::ByWarmPrefix,
        // Zero, so the threshold cannot be what excludes the boundary: only the zero-size rule itself can.
        min_shared_prefix_tokens: 0,
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st2,
        vec![affinity_row(
            &by_warm_prefix,
            "claude-sonnet-5",
            "t0",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            0,
            "root",
        )],
    );
    let main_req = placement_req(
        request(
            &by_warm_prefix,
            Some("session2"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &[],
        &["t0"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st2, &by_warm_prefix, &main_req, &mut rng()).0
    else {
        panic!("b lands the main conversation")
    };
    assert_eq!(
        sel.trace.placement, None,
        "a main conversation does not place on the zero-token boundary either"
    );
    assert_eq!(sel.account.id, "b");
}

#[test]
fn a_marker_less_portable_request_is_never_placed_by_the_static_tier() {
    let s = snapshot(Pool {
        members: vec![("a", 1), ("b", 0)],
        session_placement: SessionPlacement::ByWarmPrefix,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let req = placement_req(
        request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
        &[],
        &["s1"],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("b lands the marker-less request")
    };
    assert_eq!(sel.trace.placement, None);
    assert_eq!(sel.account.id, "b");
}

#[test]
fn a_marker_less_non_portable_request_passes_the_static_tiers_cache_gate() {
    let s = snapshot(Pool {
        members: vec![("a", 1), ("b", 0)],
        session_placement: SessionPlacement::ByWarmPrefix,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let req = placement_req(
        request(&s, Some("session"), "claude-sonnet-5", false, &[], 2_000),
        &[],
        &["s1"],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("a places the non-portable, marker-less request")
    };
    assert_eq!(
        sel.account.id, "a",
        "the cache gate passes a non-portable request even without a marker"
    );
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "static",
            depth: 1,
            prefix_tokens: 20_000,
        }),
    );
}

#[test]
fn with_main_places_a_subagents_first_request_on_the_parents_account_and_by_strategy_skips_the_lookup(
) {
    let with_main_pool = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        subagent_placement: SubagentPlacement::WithMain,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &with_main_pool,
        &request(
            &with_main_pool,
            Some("session"),
            "claude-sonnet-5",
            true,
            &["a"],
            1_000,
        ),
        &mut rng(),
    );
    assert_eq!(pinned(&st, "session").account_id, "b");
    let sub_req = subagent(
        request(
            &with_main_pool,
            Some("session"),
            "claude-sonnet-5",
            true,
            &[],
            1_500,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &with_main_pool, &sub_req, &mut rng()).0
    else {
        panic!("the subagent lands with its parent")
    };
    assert_eq!(
        sel.account.id, "b",
        "the subagent lands on its parent's account"
    );
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "with_main",
            depth: 0,
            prefix_tokens: 0,
        }),
    );

    let by_strategy_pool = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        subagent_placement: SubagentPlacement::ByStrategy,
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st2,
        &by_strategy_pool,
        &request(
            &by_strategy_pool,
            Some("session"),
            "claude-sonnet-5",
            true,
            &["a"],
            1_000,
        ),
        &mut rng(),
    );
    assert_eq!(pinned(&st2, "session").account_id, "b");
    // A live entry that by_cohort would follow, so the assertion below tells apart a genuine skip from a
    // lookup that simply found nothing.
    seed_affinity(
        &st2,
        vec![affinity_row(
            &by_strategy_pool,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "b",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let sub_req2 = subagent(
        placement_req(
            request(
                &by_strategy_pool,
                Some("session"),
                "claude-sonnet-5",
                true,
                &[],
                1_500,
            ),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st2, &by_strategy_pool, &sub_req2, &mut rng()).0
    else {
        panic!("the strategy lands the subagent")
    };
    assert_eq!(
        sel.account.id, "a",
        "by_strategy skips the lookup, so priority picks a instead of the entry's b"
    );
    assert_eq!(sel.trace.placement, None);
}

#[test]
fn with_main_reads_the_named_parent_agent_rather_than_main() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        subagent_placement: SubagentPlacement::WithMain,
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let parent = SelectRequest {
        agent_id: "planner".into(),
        ..request(&s, Some("session"), "claude-sonnet-5", true, &["a"], 1_000)
    };
    select(&mut st, &s, &parent, &mut rng());

    let sub_req = SelectRequest {
        parent_agent_id: Some("planner".into()),
        ..subagent(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
            "worker",
        )
    };
    let SelectResult::Selected(sel) = select(&mut st, &s, &sub_req, &mut rng()).0 else {
        panic!("the subagent lands with the named parent agent")
    };
    assert_eq!(
        sel.account.id, "b",
        "with_main reads the header's agent rather than defaulting to main"
    );
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "with_main",
            depth: 0,
            prefix_tokens: 0,
        }),
    );
}

#[test]
fn a_tail_match_bypasses_the_cap_and_the_static_tier_does_not() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        session_placement: SessionPlacement::ByWarmPrefix,
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let ticket = st.in_flight().dispatched("a", 1_000);
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "a",
            1_000,
            300_000,
            900,
            "root",
        )],
    );
    let content_req = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["m1"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &content_req, &mut rng()).0 else {
        panic!("a tail match still places on a")
    };
    assert_eq!(sel.account.id, "a", "a tail match bypasses the cap");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 1,
            prefix_tokens: 900,
        }),
    );
    assert_eq!(
        sel.trace.skipped,
        Vec::new(),
        "the placement's own account is not also recorded as passed over"
    );

    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            20_000,
            "root",
        )],
    );
    let static_req = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_500),
        &[],
        &["s1"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &static_req, &mut rng()).0 else {
        panic!("b lands the static-tier request")
    };
    assert_eq!(
        sel.account.id, "b",
        "the static tier respects the cap, so b takes it"
    );
    assert_eq!(sel.trace.placement, None);
    drop(ticket);
}

#[test]
fn a_tail_match_bypasses_the_ceiling_the_wind_down_and_the_floor() {
    let scenario = |pool: Pool, terms: Vec<SubscriptionTerm>, seed: &dyn Fn(&mut RoutingState)| {
        let s = snapshot_with_terms(pool, terms);
        let mut st = RoutingState::from_load(RoutingLoad::default());
        seed_affinity(
            &st,
            vec![affinity_row(
                &s,
                "claude-sonnet-5",
                "m1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            )],
        );
        seed(&mut st);
        let req = placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
            &["m1"],
            &[],
            false,
        );
        let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
            panic!("the tail match still places the request")
        };
        assert_eq!(
            sel.account.id, "a",
            "a tail match bypasses the blocker's own checks"
        );
        assert_eq!(
            sel.trace.placement,
            Some(Placement {
                rung: "content",
                depth: 1,
                prefix_tokens: 900,
            }),
        );
    };
    // Above the ceiling: b outranks a, so only the bypass can land the request there.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            select_below: Some(0.5),
            ..Pool::default()
        },
        Vec::new(),
        &|st| {
            st.record_sample("a", sample(0.5, 0.0, 1_000));
        },
    );
    // Winding down.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            failover: Failover {
                wind_down_minutes: 30,
                ..Failover::default()
            },
            ..Pool::default()
        },
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(1_000 + 10 * 60_000),
            tier: None,
        }],
        &|_| {},
    );
    // Under the Pace floor: `window_for` reads the seven-day window for this model.
    scenario(
        Pool {
            members: vec![("a", 1), ("b", 0)],
            strategy: Strategy::Pace,
            ..Pool::default()
        },
        Vec::new(),
        &|st| {
            st.record_sample("a", sample(0.0, 0.96, 1_000));
        },
    );
}

#[test]
fn after_a_return_to_plan_move_a_warm_entry_on_the_source_cannot_place_the_request() {
    let s = snapshot(cool_preference_pool(vec![("a", AccountKind::ApiKey)]));
    let mut st = loaded(vec![PinRow {
        created_at: T - MINUTE,
        ..burning("cold", "a", 0.0, false, T)
    }]);
    hot_b_cool_c(&mut st, T - 500);
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "a",
            T - 500,
            3_600_000,
            900,
            "root",
        )],
    );
    let req = placement_req(auxiliary(&s, "cold", T), &["m1"], &[], false);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("the move's destination lands the request")
    };
    assert_eq!(
        sel.account.id, "c",
        "the move's destination takes it; the source's own warm entry cannot win it back"
    );
    assert_eq!(sel.migrated, Some(MigrateReason::ReturnToPlan));
    assert_eq!(
        sel.trace.placement, None,
        "a is not among the move's destinations, so rung 3 never considers its entry"
    );
}

#[test]
fn after_a_shed_a_warm_entry_on_the_source_cannot_place_the_request() {
    let s = snapshot(shedding_pool(Shedding::default()));
    // A shed round never takes every pin on an account, so a second (unmarked) pin sits beside the one under
    // test: cold and heavier, "s1" sorts first and is the one the round marks.
    let mut st = loaded(vec![
        burning("s1", "a", 5.0, false, T),
        burning("s0", "a", 1.0, true, T),
    ]);
    hot_a_cool_b(&mut st, T - 500);
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "a",
            T - 500,
            3_600_000,
            900,
            "root",
        )],
    );
    let req = placement_req(auxiliary(&s, "s1", T), &["m1"], &[], false);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("the shed's destination lands the request")
    };
    assert_eq!(sel.account.id, "b");
    assert_eq!(sel.migrated, Some(MigrateReason::Shed));
    assert_eq!(
        sel.trace.placement, None,
        "a is the shed source and is not among the destinations rung 3 may choose"
    );
}

#[test]
fn an_expired_or_ineligible_deeper_entry_is_stepped_over_for_a_live_shared_prefix() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "m3",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "m4",
                AffinityChain::Message,
                "a",
                1_000,
                1,
                900,
                "root",
            ),
        ],
    );
    let req = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["m1", "m2", "m3", "m4"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("a lands the request at the third boundary")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement.as_ref().map(|p| p.depth),
        Some(3),
        "the fourth boundary's entry expired, so the third's live and shared match wins"
    );

    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "m3",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "m4",
                AffinityChain::Message,
                "b",
                1_000,
                300_000,
                900,
                "root",
            ),
        ],
    );
    st.record_cooldown(cooled("b", "account", 10_000, "five_hour", "rate_limit"));
    let req2 = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &["m1", "m2", "m3", "m4"],
        &[],
        false,
    );
    let SelectResult::Selected(sel2) = select(&mut st, &s, &req2, &mut rng()).0 else {
        panic!("a lands the request at the third boundary again")
    };
    assert_eq!(sel2.account.id, "a");
    assert_eq!(
        sel2.trace.placement.as_ref().map(|p| p.depth),
        Some(3),
        "the fourth's account is ineligible, so the walk steps over it to the third"
    );
}

#[test]
fn a_request_whose_session_holds_a_pin_under_another_agent_is_never_held() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("session"), "claude-sonnet-5", true, &["a"], 1_000),
        &mut rng(),
    );
    assert_eq!(pinned(&st, "session").account_id, "b");
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let sub_req = subagent(
        request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
        "worker",
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &sub_req, &mut rng()).0 else {
        panic!("the session's own pin is never held")
    };
    assert_eq!(sel.account.id, "b");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "with_main",
            depth: 0,
            prefix_tokens: 0,
        }),
    );
}

#[test]
fn the_pin_scan_steps_past_a_first_sorted_pin_on_a_cooling_account() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(pinned(&st, "session").account_id, "a");
    // "worker" sorts after "main", so an unfixed scan that takes the session's first pin still finds main's.
    let worker_req = subagent(
        request(&s, Some("session"), "claude-sonnet-5", true, &["a"], 1_100),
        "worker",
    );
    select(&mut st, &s, &worker_req, &mut rng());
    assert_eq!(pinned_agent(&st, "session", "worker").account_id, "b");
    st.record_cooldown(cooled("a", "account", 10_000, "five_hour", "rate_limit"));
    let _b = st.in_flight().dispatched("b", 1_200);
    let worker2_req = subagent(
        request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
        "worker2",
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &worker2_req, &mut rng()).0 else {
        panic!("the session's admissible pin lands the request rather than holding it")
    };
    assert_eq!(
        sel.account.id, "b",
        "main's pin sorts first but sits on a cooling account, so the scan steps to worker's"
    );
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "with_main",
            depth: 0,
            prefix_tokens: 0,
        }),
    );
}

#[test]
fn a_budget_blocked_parents_pin_never_lands_a_capped_sibling() {
    let s = with_member_budget(
        &snapshot(Pool {
            members: vec![("a", 0), ("b", 1)],
            failover: Failover {
                in_flight_cap: 1,
                ..Failover::default()
            },
            ..Pool::default()
        }),
        "a",
        None,
        Some(5.0),
    );
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_000),
        &mut rng(),
    );
    assert_eq!(pinned(&st, "session").account_id, "a");
    st.record_member_spend(
        &s,
        vec![MemberSpend {
            pool_id: "p".into(),
            account_id: "a".into(),
            period: BudgetPeriod::Month,
            period_start: 0,
            period_end: 10_000,
            spent_usd: 5.0,
            observed_at: 1_500,
        }],
    );
    let _a = st.in_flight().dispatched("a", 1_500);
    let _b = st.in_flight().dispatched("b", 1_500);
    let sub_req = subagent(
        request(&s, Some("session"), "claude-sonnet-5", true, &[], 2_000),
        "worker",
    );
    let SelectResult::Held { account_id, hold } = select(&mut st, &s, &sub_req, &mut rng()).0
    else {
        panic!("held on the strategy's choice")
    };
    assert_eq!(
        account_id, "b",
        "the parent's pin is on a member at its budget, so the request never lands there"
    );
    assert_eq!(hold.cohort, None);
}

#[test]
fn a_held_subagent_is_held_on_its_cohorts_account() {
    let s = snapshot(Pool {
        // a is priority 0, so the strategy alone would hold there too; the entry sits on b instead, so only
        // the override can move the hold.
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "b",
            1_000,
            3_600_000,
            500,
            "root",
        )],
    );
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let sub_req = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Held { account_id, hold } = select(&mut st, &s, &sub_req, &mut rng()).0
    else {
        panic!("held on the cohort's account")
    };
    assert_eq!(
        account_id, "b",
        "the override moves the hold to the cohort's account rather than the strategy's a"
    );
    assert_eq!(
        hold,
        HoldFacts {
            account_id: "b".into(),
            cohort: Some("b".into()),
            held_ms: 0,
        },
    );
}

#[test]
fn a_toolless_subagents_hold_never_follows_the_empty_tools_boundarys_cohort() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "t0",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            0,
            "root",
        )],
    );
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let sub_req = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
            &[],
            &["t0"],
            true,
        ),
        "worker",
    );
    let SelectResult::Held { account_id, hold } = select(&mut st, &s, &sub_req, &mut rng()).0
    else {
        panic!("held on the strategy's choice")
    };
    assert_eq!(account_id, "a", "the strategy still picks a by priority");
    assert_eq!(
        hold.cohort, None,
        "the empty-tools boundary carries no promised prefix, so it never names a cohort"
    );
}

/// Two members capped at one slot each, `a` first by priority, with a subagent cohort's static entry on `b`:
/// the pool the tests below hold a request in once both slots are taken.
fn capped_cohort_on_b(subagent_placement: SubagentPlacement) -> (RegistrySnapshot, RoutingState) {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        subagent_placement,
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "b",
            1_000,
            3_600_000,
            500,
            "root",
        )],
    );
    (s, st)
}

#[test]
fn a_held_subagent_under_by_strategy_waits_on_the_strategys_choice_rather_than_its_cohort() {
    let (s, mut st) = capped_cohort_on_b(SubagentPlacement::ByStrategy);
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let sub_req = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    let SelectResult::Held { account_id, hold } = select(&mut st, &s, &sub_req, &mut rng()).0
    else {
        panic!("held on the strategy's choice")
    };
    assert_eq!(
        (account_id.as_str(), hold.cohort),
        ("a", None),
        "a pool that places subagents by the strategy never names a cohort for the hold"
    );
}

#[test]
fn a_held_marker_less_portable_subagent_waits_on_the_strategys_choice_rather_than_its_cohort() {
    let (s, mut st) = capped_cohort_on_b(SubagentPlacement::ByCohort);
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let sub_req = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
            &[],
            &["s1"],
            false,
        ),
        "worker",
    );
    let SelectResult::Held { account_id, hold } = select(&mut st, &s, &sub_req, &mut rng()).0
    else {
        panic!("held on the strategy's choice")
    };
    assert_eq!(
        (account_id.as_str(), hold.cohort),
        ("a", None),
        "a request that neither carries nor receives a marker reads no cache, so no cohort's warmth is its own"
    );
}

#[test]
fn a_token_count_is_never_placed_and_lands_by_the_strategy() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "b",
            1_000,
            300_000,
            900,
            "root",
        )],
    );
    let mut count = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["m1"],
        &[],
        false,
    );
    count.count = true;
    let SelectResult::Selected(sel) = select(&mut st, &s, &count, &mut rng()).0 else {
        panic!("the count lands")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.placement),
        ("a", None),
        "a count reads no cache, so a warm message boundary on b neither places it nor shows on its trace"
    );
}

#[test]
fn a_token_count_at_the_account_cap_is_selected_and_never_held() {
    let (s, mut st) = capped_cohort_on_b(SubagentPlacement::ByCohort);
    // The session's main agent pins on b; the worker's count holds no pin of its own.
    select(
        &mut st,
        &s,
        &request(&s, Some("session"), "claude-sonnet-5", true, &["a"], 1_000),
        &mut rng(),
    );
    assert_eq!(pinned(&st, "session").account_id, "b");
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let mut count = subagent(
        placement_req(
            request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
            &[],
            &["s1"],
            true,
        ),
        "worker",
    );
    count.count = true;
    let SelectResult::Selected(sel) = select(&mut st, &s, &count, &mut rng()).0 else {
        panic!("a count is bound to no cap, so it is never held")
    };
    assert_eq!(
        sel.account.id, "a",
        "with every account at its cap the count lands where the strategy says"
    );
}

/// A count as the main agent of `s1` sends it at `now`.
fn count_of(s: &RegistrySnapshot, portable: bool, now: i64) -> SelectRequest {
    SelectRequest {
        count: true,
        ..request(s, Some("s1"), "claude-sonnet-5", portable, &[], now)
    }
}

#[test]
fn a_pinless_token_count_writes_no_pin_and_the_agents_next_message_is_placed_under_the_cap() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let _a = st.in_flight().dispatched("a", 1_000);
    let (result, writes) = select(&mut st, &s, &count_of(&s, true, 1_000), &mut rng());
    let SelectResult::Selected(sel) = result else {
        panic!("a count is bound to no cap, so it is placed: {result:?}")
    };
    assert_eq!(
        sel.pin, None,
        "a count touches no cache, so it creates no pin"
    );
    assert!(
        !writes.iter().any(|w| matches!(w, StateWrite::UpsertPin(_))),
        "{writes:?}"
    );
    assert!(st.pin(&pin_key("s1")).is_none());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
                &mut rng(),
            )
            .0
        ),
        "b",
        "the agent's next message meets the cap with no pin of the count's to renew, so it lands under it"
    );
}

#[test]
fn a_pinned_token_count_follows_its_pin_where_a_real_request_would_move() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000),
                &mut rng(),
            )
            .0
        ),
        "a"
    );
    let _a = st.in_flight().dispatched("a", 1_000);
    // Past the overage grace, so a real request would move off `a`.
    st.record_overage("a", overage(true, 1_500, Some(9 * HOUR)));
    let (result, _) = select(&mut st, &s, &count_of(&s, true, 2_000), &mut rng());
    let SelectResult::Selected(sel) = result else {
        panic!("the count lands: {result:?}")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated, sel.trace.renewed),
        ("a", None, true),
        "a count follows its pin at the cap and past a move rule"
    );
    assert_eq!(pinned(&st, "s1").account_id, "a");
    let SelectResult::Selected(sel) = select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000),
        &mut rng(),
    )
    .0
    else {
        panic!("the message lands")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Overage)),
        "the move rule still moves the agent's next message"
    );
}

#[test]
fn a_cohort_hold_that_times_out_lands_the_next_member_elsewhere() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "s1",
            AffinityChain::Static,
            "a",
            1_000,
            3_600_000,
            500,
            "root",
        )],
    );
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let sub_req = placement_req(
        SelectRequest {
            holds: vec![HoldFacts {
                account_id: "a".into(),
                cohort: Some("a".into()),
                held_ms: 1_000,
            }],
            ..subagent(
                request(&s, Some("session"), "claude-sonnet-5", true, &["a"], 1_500),
                "worker",
            )
        },
        &[],
        &["s1"],
        true,
    );
    let SelectResult::Held { account_id, hold } = select(&mut st, &s, &sub_req, &mut rng()).0
    else {
        panic!("b holds once the cohort's account is attempted and its own entry no longer admits")
    };
    assert_eq!(
        account_id, "b",
        "a is attempted, so the override skips its entry and the strategy's choice takes the hold"
    );
    assert_eq!(
        hold,
        HoldFacts {
            account_id: "b".into(),
            cohort: None,
            held_ms: 0,
        },
    );

    drop(_b);
    let follow_up = SelectRequest {
        now: 2_000,
        ..sub_req
    };
    let SelectResult::Selected(sel) = select(&mut st, &s, &follow_up, &mut rng()).0 else {
        panic!("b lands the request once it frees")
    };
    assert_eq!(sel.account.id, "b");
    assert_eq!(
        sel.trace.cohort_hold,
        Some(CohortHold {
            waited_on: vec!["a".into()],
            cohort: Some("a".into()),
            held_ms: 1_000,
        }),
    );
}

#[test]
fn a_cohort_hold_names_every_account_waited_on_the_last_cohort_and_the_waits_summed() {
    let hold = |account: &str, cohort: Option<&str>, held_ms: u64| HoldFacts {
        account_id: account.into(),
        cohort: cohort.map(String::from),
        held_ms,
    };
    assert_eq!(
        cohort_hold(&[]),
        None,
        "a request that never held names no hold"
    );
    assert_eq!(
        cohort_hold(&[hold("a", Some("a"), 1_000), hold("b", None, 250)]),
        Some(CohortHold {
            waited_on: vec!["a".into(), "b".into()],
            cohort: None,
            held_ms: 1_250,
        }),
        "the last hold's cohort, a strategy's choice here, and both waits"
    );
}

#[test]
fn the_trace_carries_the_placement_rung_depth_and_prefix() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "a",
            1_000,
            300_000,
            12_345,
            "root",
        )],
    );
    let req = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_500),
        &["m1"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("a lands the request")
    };
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 1,
            prefix_tokens: 12_345,
        }),
    );
}

#[test]
fn a_pin_both_in_overage_and_spent_records_spent_and_lands_where_both_allow() {
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
    // a is in overage past the grace and spent; b is out of overage with allowance; c is out of overage and
    // spent.
    st.record_overage("a", overage(true, 1_500, Some(9 * HOUR)));
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("c", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    let SelectResult::Selected(sel) = pick(&mut st, 2_000).0 else {
        panic!()
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "the spent rule runs first and names the move, and b is the one sibling both predicates allow"
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
    // a's five-hour window stands full while b holds allowance by a fresh sample and is in known overage.
    // Moving the pin there would hand it straight back on the next request.
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
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
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
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
    // Newer evidence retires the flag before its fallback runs out; for an account this quiet that is the poll's
    // next reading.
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
        [PinKey {
            key_id: "k".to_string(),
            session_id: "s1".to_string(),
            agent_id: "main".to_string(),
            model: "claude-sonnet-4-5".to_string()
        }],
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
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        account_id: "a".into(),
        epoch: 1,
        created_at: 0,
        last_used_at: 0,
        key_generation: 0,
        incarnation: 1,
        burn: PinBurn::default(),
        burn_at: None,
        burn_banded: PinBurn::default(),
        last_input_tokens: 0,
        last_refresh_at: 0,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
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
    // a's window is half used with a reset a moment behind now, exactly the case a minute's clamp on hours
    // would rank first on an inflated score (0.5 over a minute is 30, dwarfing any real remaining allowance).
    st.record_sample(
        "a",
        UtilizationSample {
            observed_at: 999,
            ..Default::default()
        }
        .with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.5,
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
    // a's raw score (0.05 remaining over a live 1h reset) beats c's (0.5 remaining over the untouched 168h
    // default, ~0.003), so only the floor keeps c ahead of it; without the floor a would win outright.
    st.record_sample(
        "a",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            SEVEN_DAY,
            WindowReading {
                utilization: 0.95,
                reset: Some(1_000 + HOUR),
                observed_at: 1_000,
            },
        ),
    );
    st.record_sample("b", sample(0.0, 0.92, 1_000)); // remaining 0.08, below the floor
    st.record_sample("c", sample(0.0, 0.5, 1_000)); // remaining 0.5, above the floor
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        sel.account.id, "c",
        "a's raw score would win without the floor, so only the floor keeps c, the sole account above it, ahead"
    );
    assert!(
        sel.trace
            .skipped
            .iter()
            .any(|s| s.account_id == "a" && s.obstacle == "floor"),
        "{:?}",
        sel.trace.skipped
    );

    // All three below the floor: the floor partition keeps the wider set, and pace ranks among them by real
    // score, a's live reset included.
    st.record_sample("c", sample(0.0, 0.97, 1_100)); // remaining 0.03, now below the floor too
    let req2 = request(&s, None, "claude-sonnet-5", true, &[], 1_100);
    assert_eq!(
        picked(&select(&mut st, &s, &req2, &mut rng()).0),
        "a",
        "nothing is above the floor, so pace places a, whose live-reset score still beats b's and c's"
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
            tier: None,
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
            tier: None,
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
            tier: None,
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
        .pin(&pin_key("s1"))
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
            tier: None,
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
            tier: None,
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

#[test]
fn every_candidate_at_its_cap_answers_held_with_the_best_of_them() {
    let s = snapshot(Pool {
        members: vec![("a", 1), ("b", 0), ("c", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let mut tickets: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|id| st.in_flight().dispatched(id, 1_000))
        .collect();
    let req = request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000);
    let (result, writes) = select(&mut st, &s, &req, &mut rng());
    assert_eq!(
        result,
        SelectResult::Held {
            account_id: "b".into(),
            hold: HoldFacts {
                account_id: "b".into(),
                cohort: None,
                held_ms: 0,
            },
        },
        "b is the one priority would have chosen"
    );
    assert!(
        writes.is_empty(),
        "a held answer places nothing: {writes:?}"
    );
    assert!(
        st.pin(&pin_key("s1")).is_none(),
        "no pin is made for a request that is held"
    );
    drop(tickets.remove(2));
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "c",
        "a slot on any candidate ends the hold"
    );
}

#[test]
fn a_held_round_robin_selection_leaves_the_turn_with_the_account_it_named() {
    let s = snapshot(Pool {
        strategy: Strategy::RoundRobin,
        members: vec![("a", 0), ("b", 0)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let tickets: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|id| st.in_flight().dispatched(id, 1_000))
        .collect();
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    assert_eq!(
        select(&mut st, &s, &req, &mut rng()).0,
        SelectResult::Held {
            account_id: "a".into(),
            hold: HoldFacts {
                account_id: "a".into(),
                cohort: None,
                held_ms: 0,
            },
        },
        "the round starts at a"
    );
    drop(tickets);
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "a",
        "the hold placed nothing, so the turn is still a's"
    );
    assert_eq!(
        picked(&select(&mut st, &s, &req, &mut rng()).0),
        "b",
        "the placement on a moved the turn on"
    );
}

#[test]
fn the_cap_binds_before_the_ceiling_so_a_free_account_above_it_takes_the_placement() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        select_below: Some(0.5),
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    st.record_sample("a", sample(0.2, 0.1, 1_000));
    st.record_sample("b", sample(0.9, 0.1, 1_000));
    let on_a = st.in_flight().dispatched("a", 1_000);
    let req = request(&s, None, "claude-sonnet-5", true, &[], 1_000);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!("a free account takes the placement")
    };
    assert_eq!(
        sel.account.id, "b",
        "the below-ceiling account is at its cap, so the free one above the ceiling takes it"
    );
    assert!(
        sel.trace
            .skipped
            .iter()
            .any(|s| s.account_id == "a" && s.obstacle == "at_cap"),
        "{:?}",
        sel.trace.skipped
    );
    let on_b = st.in_flight().dispatched("b", 1_000);
    assert_eq!(
        select(&mut st, &s, &req, &mut rng()).0,
        SelectResult::Held {
            account_id: "a".into(),
            hold: HoldFacts {
                account_id: "a".into(),
                cohort: None,
                held_ms: 0,
            },
        },
        "with both at their cap the request is held on the one the ceiling prefers"
    );
    drop((on_a, on_b));
}

#[test]
fn a_move_whose_every_destination_is_at_its_cap_is_not_made_and_the_pin_renews_in_place() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
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
    let epoch = st.pin(&pin_key("s1")).unwrap().epoch;
    // a's window is spent and b holds allowance, so the spent move is due; b is at its cap.
    st.record_sample("a", refreshing(1.0, 9 * HOUR, 9 * HOUR, 1_500));
    st.record_sample("b", refreshing(0.4, 9 * HOUR, 9 * HOUR, 1_500));
    let on_b = st.in_flight().dispatched("b", 1_500);
    let (result, writes) = pick(&mut st, 2_000);
    let SelectResult::Selected(sel) = result else {
        panic!("the pin renews in place rather than holding: {result:?}")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated, sel.trace.renewed),
        ("a", None, true),
        "no move is made onto a destination set that is entirely at its cap"
    );
    assert!(
        !writes
            .iter()
            .any(|w| matches!(w, StateWrite::DeletePin(..))),
        "the pin is never deleted, so no release is reported: {writes:?}"
    );
    assert_eq!(st.pin(&pin_key("s1")).unwrap().epoch, epoch);
    drop(on_b);
    let SelectResult::Selected(sel) = pick(&mut st, 3_000).0 else {
        panic!("b has a slot again")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.migrated),
        ("b", Some(MigrateReason::Spent)),
        "the move is judged again at the next request"
    );
}

#[test]
fn a_fable_spent_candidate_is_skipped_as_spent_whether_or_not_it_names_a_reset() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 0), ("c", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // a: spent with a reset, the case that read as "cooldown" in the trace.
    st.record_sample(
        "a",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            SEVEN_DAY_FABLE,
            WindowReading {
                utilization: 1.0,
                reset: Some(2_000),
                observed_at: 1_000,
            },
        ),
    );
    // b: spent with no reset, the case the trace carried nothing for at all.
    st.record_sample(
        "b",
        UtilizationSample {
            observed_at: 1_000,
            ..Default::default()
        }
        .with(
            SEVEN_DAY_FABLE,
            WindowReading {
                utilization: 1.0,
                reset: None,
                observed_at: 1_000,
            },
        ),
    );
    let req = request(&s, None, "claude-fable-5-1", true, &[], 1_000);
    let SelectResult::Selected(sel) = select(&mut st, &s, &req, &mut rng()).0 else {
        panic!()
    };
    assert_eq!(
        sel.account.id, "c",
        "a and b are both spent, so c is the only one left to choose"
    );
    for id in ["a", "b"] {
        assert!(
            sel.trace
                .skipped
                .iter()
                .any(|sk| sk.account_id == id && sk.obstacle == "spent"),
            "{id}: {:?}",
            sel.trace.skipped
        );
    }
}

/// A clock well past the cache TTL, so a refresh zeroed by a cold verdict reads cold.
const T: i64 = 10 * HOUR;

/// `req` carrying the hint headers and the two chains the relay reads off the client's bytes.
fn with_facts(
    req: SelectRequest,
    class: Option<&str>,
    compacted: bool,
    static_hash: Option<&str>,
    boundaries: &[&str],
) -> SelectRequest {
    SelectRequest {
        request_class: class.map(String::from),
        context_compacted: compacted,
        static_hash: static_hash.map(String::from),
        boundaries: boundaries.iter().map(|b| b.to_string()).collect(),
        ..req
    }
}

fn pin_key(session: &str) -> PinKey {
    PinKey {
        key_id: "k".into(),
        session_id: session.into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
    }
}

fn pinned(st: &RoutingState, session: &str) -> PinRow {
    st.pin(&pin_key(session))
        .cloned()
        .expect("the session holds a pin")
}

/// As `pinned`, for an agent other than `main`.
fn pinned_agent(st: &RoutingState, session: &str, agent_id: &str) -> PinRow {
    st.pin(&PinKey {
        agent_id: agent_id.into(),
        ..pin_key(session)
    })
    .cloned()
    .expect("the session holds a pin under this agent")
}

#[test]
fn a_main_or_subagent_or_compaction_class_refreshes_a_pin_and_any_other_class_leaves_it_alone() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64, class: &str| {
        with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            Some(class),
            false,
            None,
            &[],
        )
    };
    select(&mut st, &s, &at(1_000, "main"), &mut rng());
    assert_eq!(pinned(&st, "s1").last_refresh_at, 1_000);
    let mut now = 1_000;
    for class in ["main", "subagent", "compaction"] {
        now += 1_000;
        select(&mut st, &s, &at(now, class), &mut rng());
        assert_eq!(
            pinned(&st, "s1").last_refresh_at,
            now,
            "a {class} turn refreshes the pin"
        );
    }
    let refreshed = now;
    for class in ["quota", "title", "suggestion"] {
        now += 1_000;
        select(&mut st, &s, &at(now, class), &mut rng());
        let pin = pinned(&st, "s1");
        assert_eq!(
            (pin.last_refresh_at, pin.last_used_at),
            (refreshed, now),
            "a {class} request uses the pin and leaves its warmth alone"
        );
    }
}

#[test]
fn without_the_class_a_continuation_refreshes_a_restart_is_cold_and_a_foreign_request_changes_nothing(
) {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |now: i64, static_hash: &str, boundaries: &[&str]| {
        with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            None,
            false,
            Some(static_hash),
            boundaries,
        )
    };
    select(&mut st, &s, &at(T + 1_000, "s0", &["m1", "m2"]), &mut rng());
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.static_hash.as_deref(), pin.tail_hash.as_deref()),
        (Some("s0"), Some("m2")),
        "a fresh pin carries the request's hashes"
    );
    select(
        &mut st,
        &s,
        &at(T + 2_000, "s0", &["m1", "m2", "m3", "m4"]),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.last_refresh_at, pin.tail_hash.as_deref()),
        (T + 2_000, Some("m4")),
        "the boundaries contain the pin's tail, so the request continues it"
    );
    select(&mut st, &s, &at(T + 3_000, "s0", &["x1"]), &mut rng());
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.last_refresh_at, pin.tail_hash.as_deref()),
        (0, Some("x1")),
        "the same static prefix without the tail restarted the conversation, which is cold"
    );
    assert!(!warm(&pin, T + 3_000));
    select(&mut st, &s, &at(T + 4_000, "other", &["y1"]), &mut rng());
    let foreign = pinned(&st, "s1");
    assert_eq!(
        (
            foreign.last_refresh_at,
            foreign.static_hash.as_deref(),
            foreign.tail_hash.as_deref(),
            foreign.last_used_at
        ),
        (0, Some("s0"), Some("x1"), T + 4_000),
        "a request matching neither hash changes nothing but the pin's use"
    );
    select(&mut st, &s, &at(T + 5_000, "s0", &["x1", "x2"]), &mut rng());
    assert!(
        warm(&pinned(&st, "s1"), T + 5_000),
        "the restarted conversation's continuation warms the pin again"
    );
}

#[test]
fn a_compacted_request_is_cold_and_the_next_continuation_re_warms_the_pin() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let req = |now: i64| request(&s, Some("s1"), "claude-sonnet-5", true, &[], now);
    select(
        &mut st,
        &s,
        &with_facts(req(T + 1_000), Some("main"), false, Some("s0"), &["m1"]),
        &mut rng(),
    );
    select(
        &mut st,
        &s,
        &with_facts(req(T + 2_000), Some("main"), true, Some("s0"), &["c1"]),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.last_refresh_at, pin.tail_hash.as_deref()),
        (0, Some("c1")),
        "the compacted flag is cold whatever the class says, and the pin keeps the compacted chain's tail"
    );
    assert!(!warm(&pin, T + 2_000));
    select(
        &mut st,
        &s,
        &with_facts(req(T + 3_000), None, false, Some("s0"), &["c1", "c2"]),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(pin.last_refresh_at, T + 3_000);
    assert!(
        warm(&pin, T + 3_000),
        "the continuation of the compacted conversation re-warms the pin"
    );
}

#[test]
fn a_body_without_a_chain_refreshes_on_every_request() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    // A chained first request, so the pin holds hashes a chainless body could otherwise be judged against.
    select(
        &mut st,
        &s,
        &with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
            None,
            false,
            Some("s0"),
            &["m1"],
        ),
        &mut rng(),
    );
    for now in [T + 1_000, T + 2_000] {
        select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            &mut rng(),
        );
        let pin = pinned(&st, "s1");
        assert_eq!(
            (
                pin.last_refresh_at,
                pin.static_hash.as_deref(),
                pin.tail_hash.as_deref()
            ),
            (now, Some("s0"), Some("m1")),
            "a request that carries no fact refreshes the pin and keeps its hashes"
        );
    }
}

#[test]
fn a_probe_never_colds_a_pin() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let req = |now: i64| request(&s, Some("s1"), "claude-sonnet-5", true, &[], now);
    select(
        &mut st,
        &s,
        &with_facts(req(T + 1_000), Some("main"), false, Some("s0"), &["m1"]),
        &mut rng(),
    );
    let probes = [
        (
            "compacted",
            with_facts(req(T + 2_000), Some("main"), true, Some("s0"), &["m1"]),
        ),
        (
            "restart-shaped",
            with_facts(req(T + 3_000), None, false, Some("s0"), &["x1"]),
        ),
        (
            "foreign",
            with_facts(req(T + 4_000), None, false, Some("other"), &["y1"]),
        ),
    ];
    for (shape, probe) in probes {
        let probe = SelectRequest {
            probe: true,
            ..probe
        };
        let before = pinned(&st, "s1");
        assert!(
            effective_warm(&before, &probe, &st.affinity().read().unwrap()),
            "{shape}: the round's order reads the pin warm on the probe"
        );
        select(&mut st, &s, &probe, &mut rng());
        let after = pinned(&st, "s1");
        assert!(
            after.last_refresh_at >= before.last_refresh_at,
            "{shape}: {} fell back from {}",
            after.last_refresh_at,
            before.last_refresh_at
        );
        assert!(
            warm(&after, probe.now),
            "{shape}: still warm after the probe"
        );
    }
}

#[test]
fn a_request_on_another_lane_is_cold_whatever_its_boundaries_and_its_continuation_there_refreshes()
{
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let standard = lane_key("claude-sonnet-5", "standard", "none", "-");
    let fast = lane_key("claude-sonnet-5", "fast", "none", "-");
    let at = |now: i64, lane: &str, class: Option<&str>, boundaries: &[&str]| SelectRequest {
        lane: lane.to_string(),
        ..with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            class,
            false,
            Some("s0"),
            boundaries,
        )
    };
    select(
        &mut st,
        &s,
        &at(T + 1_000, &standard, None, &["m1", "m2"]),
        &mut rng(),
    );
    assert_eq!(
        pinned(&st, "s1").lane,
        standard,
        "a fresh pin takes the request's lane"
    );
    select(
        &mut st,
        &s,
        &at(T + 2_000, &fast, None, &["m1", "m2", "m3"]),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (
            pin.last_refresh_at,
            pin.lane.as_str(),
            pin.tail_hash.as_deref()
        ),
        (0, fast.as_str(), Some("m3")),
        "the boundaries continue the pin's tail, but another lane is another cache"
    );
    select(
        &mut st,
        &s,
        &at(T + 3_000, &fast, None, &["m1", "m2", "m3", "m4"]),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.last_refresh_at, pin.tail_hash.as_deref()),
        (T + 3_000, Some("m4")),
        "the continuation on the new lane refreshes"
    );
    let five = ["m1", "m2", "m3", "m4", "m5"];
    select(
        &mut st,
        &s,
        &at(T + 4_000, &standard, Some("quota"), &five),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (
            pin.last_refresh_at,
            pin.lane.as_str(),
            pin.tail_hash.as_deref()
        ),
        (T + 3_000, fast.as_str(), Some("m4")),
        "an auxiliary class on another lane leaves the pin as it was"
    );
    select(
        &mut st,
        &s,
        &at(T + 5_000, &standard, Some("main"), &five),
        &mut rng(),
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (
            pin.last_refresh_at,
            pin.lane.as_str(),
            pin.tail_hash.as_deref()
        ),
        (0, standard.as_str(), Some("m5")),
        "a main turn on another lane reads cold too"
    );
}

#[test]
fn a_request_that_continues_nothing_on_another_lane_leaves_a_shared_identitys_pin_untouched() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let standard = lane_key("claude-sonnet-5", "standard", "none", "-");
    let fast_lane = lane_key("claude-sonnet-5", "fast", "none", "-");
    let at = |now: i64, lane: &str, static_hash: Option<&str>, boundaries: &[&str]| SelectRequest {
        lane: lane.to_string(),
        ..with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            None,
            false,
            static_hash,
            boundaries,
        )
    };
    select(
        &mut st,
        &s,
        &at(T + 1_000, &standard, Some("s0"), &["m1", "m2"]),
        &mut rng(),
    );
    let before = pinned(&st, "s1");
    select(
        &mut st,
        &s,
        &at(T + 2_000, &fast_lane, Some("s9"), &["x1", "x2"]),
        &mut rng(),
    );
    let after = pinned(&st, "s1");
    assert_eq!(
        (
            after.lane.as_str(),
            after.last_refresh_at,
            after.tail_hash.as_deref()
        ),
        (standard.as_str(), before.last_refresh_at, Some("m2")),
        "another conversation beside the pin's, whatever its lane, changes nothing"
    );
    let map = st.affinity();
    let map = map.read().unwrap();
    assert_eq!(
        warmth_of(&after, &at(T + 3_000, &fast_lane, None, &[]), &map),
        Warmth::Cold,
        "a chainless body on another lane still reads cold"
    );
    assert_eq!(
        warmth_of(
            &after,
            &at(T + 3_000, &fast_lane, Some("s0"), &["m1", "m2", "m3"]),
            &map
        ),
        Warmth::Cold,
        "and so does a continuation"
    );
}

#[test]
fn a_control_entry_after_the_tail_continues_the_conversation() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let per_turn = lane_key("claude-sonnet-5", "standard", "adaptive", "*");
    let at = |now: i64, boundaries: &[&str]| SelectRequest {
        lane: per_turn.clone(),
        ..with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], now),
            None,
            false,
            Some("s0"),
            boundaries,
        )
    };
    select(&mut st, &s, &at(T + 1_000, &["m1", "m2"]), &mut rng());
    let with_control_entry = at(T + 2_000, &["m1", "m2", "c1"]);
    assert_eq!(
        warmth_of(
            &pinned(&st, "s1"),
            &with_control_entry,
            &st.affinity().read().unwrap()
        ),
        Warmth::Refresh,
        "the control entry is one more message past the pin's tail"
    );
    select(&mut st, &s, &with_control_entry, &mut rng());
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.last_refresh_at, pin.tail_hash.as_deref()),
        (T + 2_000, Some("c1"))
    );
}

#[test]
fn a_fresh_request_finds_no_warm_entry_of_another_lane_and_lands_by_the_strategy() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let standard = lane_key("claude-sonnet-5", "standard", "none", "-");
    let fast = lane_key("claude-sonnet-5", "fast", "none", "-");
    // The entry sits on b, behind a by priority, so only a placement lands the request there.
    let warm_on_b = || {
        let st = RoutingState::from_load(RoutingLoad::default());
        seed_affinity(
            &st,
            vec![affinity_row(
                &s,
                "claude-sonnet-5",
                "m1",
                AffinityChain::Message,
                "b",
                1_000,
                300_000,
                12_345,
                "root",
            )],
        );
        st
    };
    let fresh = |lane: &str| SelectRequest {
        lane: lane.to_string(),
        ..with_facts(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_500),
            None,
            false,
            None,
            &["m1", "m2"],
        )
    };
    let SelectResult::Selected(sel) = select(&mut warm_on_b(), &s, &fresh(&fast), &mut rng()).0
    else {
        panic!("a lands the request")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.placement),
        ("a", None),
        "the standard lane's entry is no warm prefix for a fast request"
    );
    let SelectResult::Selected(sel) = select(&mut warm_on_b(), &s, &fresh(&standard), &mut rng()).0
    else {
        panic!("b lands the request")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.placement.map(|p| p.rung)),
        ("b", Some("content")),
        "on its own lane the entry places the request"
    );

    let static_subagent = |lane: &str| SelectRequest {
        lane: lane.to_string(),
        ..subagent(
            placement_req(
                request(&s, Some("session"), "claude-sonnet-5", true, &[], 1_500),
                &[],
                &["s1"],
                true,
            ),
            "worker",
        )
    };
    let (s, mut st) = capped_cohort_on_b(SubagentPlacement::ByCohort);
    let SelectResult::Selected(sel) = select(&mut st, &s, &static_subagent(&fast), &mut rng()).0
    else {
        panic!("a lands the subagent")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.placement),
        ("a", None),
        "nor is a static entry of another lane"
    );
    let (s, mut st) = capped_cohort_on_b(SubagentPlacement::ByCohort);
    let SelectResult::Selected(sel) =
        select(&mut st, &s, &static_subagent(&standard), &mut rng()).0
    else {
        panic!("b lands the subagent")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.placement.map(|p| p.rung)),
        ("b", Some("static"))
    );
    let (s, mut st) = capped_cohort_on_b(SubagentPlacement::ByCohort);
    let _a = st.in_flight().dispatched("a", 1_000);
    let _b = st.in_flight().dispatched("b", 1_000);
    let SelectResult::Held { account_id, hold } =
        select(&mut st, &s, &static_subagent(&fast), &mut rng()).0
    else {
        panic!("held on the strategy's choice")
    };
    assert_eq!(
        (account_id.as_str(), hold.cohort),
        ("a", None),
        "a cohort of another lane holds no subagent on its account"
    );
}

#[test]
fn a_pin_and_an_entry_written_before_the_lane_existed_adopt_the_requests_lane_and_stay_warm() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let standard = lane_key("claude-sonnet-5", "standard", "none", "-");
    let fast = lane_key("claude-sonnet-5", "fast", "none", "-");
    let turn = |session: &str, now: i64, lane: &str, boundaries: &[&str]| SelectRequest {
        lane: lane.to_string(),
        ..with_facts(
            request(&s, Some(session), "claude-sonnet-5", true, &[], now),
            Some("main"),
            false,
            None,
            boundaries,
        )
    };
    // A warm pin the upgrade left without a lane, on b behind a by priority, so only the pin keeps it there.
    let mut st = loaded(vec![PinRow {
        tail_hash: Some("m1".into()),
        lane: String::new(),
        ..seeded_pin("s1", "b", T)
    }]);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &turn("s1", T, &standard, &["m1", "m2"]),
                &mut rng()
            )
            .0
        ),
        ("b".into(), None)
    );
    let pin = pinned(&st, "s1");
    assert_eq!(
        (pin.last_refresh_at, pin.lane.as_str()),
        (T, standard.as_str()),
        "its first request after the upgrade refreshes it and stamps the request's lane"
    );
    select(
        &mut st,
        &s,
        &turn("s1", T + 1_000, &fast, &["m1", "m2", "m3"]),
        &mut rng(),
    );
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        0,
        "from then on it holds that lane, so another one reads cold"
    );

    // An entry the upgrade left without a lane places a fresh request on either lane until a report stamps it.
    let entry_on_b = || {
        let st = RoutingState::from_load(RoutingLoad::default());
        seed_affinity(
            &st,
            vec![AffinityRow {
                lane: String::new(),
                ..affinity_row(
                    &s,
                    "claude-sonnet-5",
                    "m1",
                    AffinityChain::Message,
                    "b",
                    T - 1_000,
                    300_000,
                    12_345,
                    "root",
                )
            }],
        );
        st
    };
    for lane in [&standard, &fast] {
        let SelectResult::Selected(sel) = select(
            &mut entry_on_b(),
            &s,
            &turn("s2", T, lane, &["m1", "m2"]),
            &mut rng(),
        )
        .0
        else {
            panic!("b lands the request")
        };
        assert_eq!(
            (sel.account.id.as_str(), sel.trace.placement.map(|p| p.rung)),
            ("b", Some("content")),
            "{lane}: the lane-less entry is the request's own"
        );
    }
}

#[test]
fn a_resolution_between_two_turns_keeps_the_pin_warm_on_the_dated_id() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let turn = |model: &str, now: i64, boundaries: &[&str]| {
        with_facts(
            request(&s, Some("s1"), model, true, &[], now),
            Some("main"),
            false,
            None,
            boundaries,
        )
    };
    select(
        &mut st,
        &s,
        &turn("claude-sonnet-4-5", T, &["m1"]),
        &mut rng(),
    );
    let rekey = st.apply_resolution("claude-sonnet-4-5", "claude-sonnet-4-5-20250929", T + 500);
    let dated_lane = lane_key("claude-sonnet-4-5-20250929", "standard", "none", "-");
    assert_eq!(
        rekey
            .upsert_pins
            .iter()
            .map(|p| p.lane.as_str())
            .collect::<Vec<_>>(),
        [dated_lane.as_str()],
        "the moved pin's lane names the model the next request composes it from"
    );
    select(
        &mut st,
        &s,
        &turn("claude-sonnet-4-5-20250929", T + 1_000, &["m1", "m2"]),
        &mut rng(),
    );
    let pin = st
        .pin(&PinKey {
            model: "claude-sonnet-4-5-20250929".into(),
            ..pin_key("s1")
        })
        .cloned()
        .expect("the pin moved to the dated id");
    assert_eq!(
        pin.last_refresh_at,
        T + 1_000,
        "the continuation after the resolution refreshes its pin"
    );
}

/// A state whose session "s1" already holds a pin on "a", made by a real selection with static prefix "s0" and
/// tail "old-tail": the fixture the warmth tests below build on.
fn pinned_state(s: &RegistrySnapshot, now: i64) -> RoutingState {
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let first = placement_req(
        request(s, Some("s1"), "claude-sonnet-5", true, &[], now),
        &["old-tail"],
        &["s0"],
        false,
    );
    select(&mut st, s, &first, &mut rng());
    st
}

#[test]
fn a_restart_under_a_session_id_reads_cold() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = pinned_state(&s, 1_000);
    let restarted = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["y1"],
        &["s0"],
        false,
    );
    select(&mut st, &s, &restarted, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        0,
        "the same static prefix with fresh messages under a session identity is cold"
    );
}

#[test]
fn two_conversations_alternating_on_one_pin_each_refresh_it() {
    let s = snapshot(Pool {
        members: vec![("a", 0)],
        ..Pool::default()
    });
    let mut st = pinned_state(&s, 1_000);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "a1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "rootA",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "rootB",
            ),
        ],
    );
    let mut b_continues = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2"],
        &[],
        false,
    );
    select(&mut st, &s, &b_continues, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        2_000,
        "b's continuation refreshes though the pin's tail is a's own boundary"
    );

    let mut a_continues = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000),
        &["a1", "a2"],
        &[],
        false,
    );
    select(&mut st, &s, &a_continues, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        3_000,
        "a's continuation refreshes though the tail is now b's"
    );
}

#[test]
fn a_map_entry_on_another_account_key_or_incarnation_does_not_refresh_the_pin() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    // A static-only match reads cold once the map's entry is ruled out, so a refresh here would show the map
    // wrongly counted.
    let followup = |st: &mut RoutingState| -> i64 {
        let req = placement_req(
            request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
            &["m1"],
            &["s0"],
            false,
        );
        select(st, &s, &req, &mut rng());
        pinned(st, "s1").last_refresh_at
    };

    let mut under_sibling = pinned_state(&s, 1_000);
    seed_affinity(
        &under_sibling,
        vec![affinity_row(
            &s,
            "claude-sonnet-5",
            "m1",
            AffinityChain::Message,
            "b",
            1_000,
            300_000,
            900,
            "root",
        )],
    );
    assert_eq!(
        followup(&mut under_sibling), 0,
        "the sibling's entry does not refresh the pin, so the static-only match reads cold under a session identity"
    );

    let mut under_other_key = pinned_state(&s, 1_000);
    let mut row = affinity_row(
        &s,
        "claude-sonnet-5",
        "m1",
        AffinityChain::Message,
        "a",
        1_000,
        300_000,
        900,
        "root",
    );
    row.key_id = "other".into();
    seed_affinity(&under_other_key, vec![row]);
    assert_eq!(
        followup(&mut under_other_key),
        0,
        "an entry another key wrote does not refresh the pin either"
    );

    let mut under_old_incarnation = pinned_state(&s, 1_000);
    let mut row = affinity_row(
        &s,
        "claude-sonnet-5",
        "m1",
        AffinityChain::Message,
        "a",
        1_000,
        300_000,
        900,
        "root",
    );
    row.incarnation = 0;
    seed_affinity(&under_old_incarnation, vec![row]);
    assert_eq!(
        followup(&mut under_old_incarnation),
        0,
        "an entry under an older incarnation does not refresh the pin either"
    );
}

#[test]
fn a_continuation_refreshes_its_pin_and_places_by_content_at_the_previous_tails_depth() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        tail_hash: Some("b3".into()),
        last_refresh_at: 0,
        ..seeded_pin("s1", "a", 1_000)
    }]);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b2",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b3",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
        ],
    );
    let continuation = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4"],
        &[],
        false,
    );
    select(&mut st, &s, &continuation, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        2_000,
        "the continuation refreshes the pin"
    );

    let fresh = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &fresh, &mut rng()).0 else {
        panic!("a lands the fresh request at the previous tail's depth")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 3,
            prefix_tokens: 900
        }),
    );
}

#[test]
fn a_fork_refreshes_its_pin_and_places_by_content_at_the_last_shared_boundary() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        tail_hash: Some("b4".into()),
        last_refresh_at: 0,
        ..seeded_pin("s1", "a", 1_000)
    }]);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b2",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b3",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
        ],
    );
    let fork = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "x4"],
        &[],
        false,
    );
    select(&mut st, &s, &fork, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        2_000,
        "the map's b3 entry refreshes the pin though its own tail (b4) is absent from the fork"
    );

    let fresh = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "x4"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &fresh, &mut rng()).0 else {
        panic!("a lands the fresh request at the last shared boundary")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 3,
            prefix_tokens: 900
        }),
    );
}

#[test]
fn an_edited_last_message_refreshes_its_pin_and_places_by_content_one_boundary_shallower() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        tail_hash: Some("b4".into()),
        last_refresh_at: 0,
        ..seeded_pin("s1", "a", 1_000)
    }]);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b2",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b3",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
        ],
    );
    let retry = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4-edited"],
        &[],
        false,
    );
    select(&mut st, &s, &retry, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        2_000,
        "the map's b3 entry refreshes the pin though the edited last message is not b4"
    );

    let fresh = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4-edited"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &fresh, &mut rng()).0 else {
        panic!("a lands the fresh request one boundary shallower")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 3,
            prefix_tokens: 900
        }),
    );
}

#[test]
fn the_same_static_prefix_with_fresh_messages_is_cold_on_its_pin_and_places_by_static_only_under_the_knob(
) {
    let by_strategy = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        static_hash: Some("s2".into()),
        tail_hash: Some("m1".into()),
        last_refresh_at: 500,
        ..seeded_pin("s1", "a", 1_000)
    }]);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &by_strategy,
                "claude-sonnet-5",
                "s1",
                AffinityChain::Static,
                "a",
                1_000,
                3_600_000,
                20_000,
                "root",
            ),
            affinity_row(
                &by_strategy,
                "claude-sonnet-5",
                "s2",
                AffinityChain::Static,
                "a",
                1_000,
                3_600_000,
                20_000,
                "root",
            ),
        ],
    );
    let restarted = placement_req(
        request(
            &by_strategy,
            Some("s1"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &["y1"],
        &["s1", "s2"],
        true,
    );
    select(&mut st, &by_strategy, &restarted, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        0,
        "the same static prefix with fresh messages is cold on its pin"
    );

    let fresh = placement_req(
        request(
            &by_strategy,
            Some("s2"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &["y1"],
        &["s1", "s2"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st, &by_strategy, &fresh, &mut rng()).0 else {
        panic!("b lands the fresh request under by_strategy")
    };
    assert_eq!(
        sel.trace.placement, None,
        "by_strategy does not use the static tier for a main conversation"
    );

    let by_warm_prefix = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        session_placement: SessionPlacement::ByWarmPrefix,
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st2,
        vec![
            affinity_row(
                &by_warm_prefix,
                "claude-sonnet-5",
                "s1",
                AffinityChain::Static,
                "a",
                1_000,
                3_600_000,
                20_000,
                "root",
            ),
            affinity_row(
                &by_warm_prefix,
                "claude-sonnet-5",
                "s2",
                AffinityChain::Static,
                "a",
                1_000,
                3_600_000,
                20_000,
                "root",
            ),
        ],
    );
    let fresh2 = placement_req(
        request(
            &by_warm_prefix,
            Some("s3"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &["y1"],
        &["s1", "s2"],
        true,
    );
    let SelectResult::Selected(sel2) = select(&mut st2, &by_warm_prefix, &fresh2, &mut rng()).0
    else {
        panic!("a lands the fresh request under by_warm_prefix")
    };
    assert_eq!(sel2.account.id, "a");
    assert_eq!(
        sel2.trace.placement,
        Some(Placement {
            rung: "static",
            depth: 2,
            prefix_tokens: 20_000
        }),
    );
}

#[test]
fn a_tools_change_mid_session_refreshes_by_the_message_chain_and_places_by_content() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        tail_hash: Some("b3".into()),
        static_hash: Some("s-old".into()),
        last_refresh_at: 0,
        ..seeded_pin("s1", "a", 1_000)
    }]);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b1",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b2",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
            affinity_row(
                &s,
                "claude-sonnet-5",
                "b3",
                AffinityChain::Message,
                "a",
                1_000,
                300_000,
                900,
                "root",
            ),
        ],
    );
    let changed = placement_req(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4"],
        &["s3"],
        true,
    );
    select(&mut st, &s, &changed, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        2_000,
        "the message chain still refreshes the pin though the static chain moved"
    );

    let fresh = placement_req(
        request(&s, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4"],
        &["s3"],
        true,
    );
    let SelectResult::Selected(sel) = select(&mut st, &s, &fresh, &mut rng()).0 else {
        panic!("a lands the fresh request by content")
    };
    assert_eq!(sel.account.id, "a");
    assert_eq!(
        sel.trace.placement,
        Some(Placement {
            rung: "content",
            depth: 3,
            prefix_tokens: 900
        }),
        "the moved static chain (s3) has no entry, so only the message chain places it"
    );
}

#[test]
fn a_marker_less_conversation_refreshes_its_pin_under_always_and_places_by_the_strategy_under_both_modes(
) {
    let always = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        mode: AffinityMode::Always,
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        tail_hash: Some("b3".into()),
        last_refresh_at: 0,
        ..seeded_pin("s1", "a", 1_000)
    }]);
    seed_affinity(
        &st,
        vec![
            affinity_row(
                &always,
                "claude-sonnet-5",
                "b1",
                AffinityChain::Message,
                "a",
                1_000,
                0,
                0,
                "root",
            ),
            affinity_row(
                &always,
                "claude-sonnet-5",
                "b2",
                AffinityChain::Message,
                "a",
                1_000,
                0,
                0,
                "root",
            ),
            affinity_row(
                &always,
                "claude-sonnet-5",
                "b3",
                AffinityChain::Message,
                "a",
                1_000,
                0,
                0,
                "root",
            ),
        ],
    );
    let continuation = placement_req(
        request(&always, Some("s1"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4"],
        &[],
        false,
    );
    select(&mut st, &always, &continuation, &mut rng());
    assert_eq!(
        pinned(&st, "s1").last_refresh_at,
        2_000,
        "the marker-less continuation still refreshes the pinned conversation under always"
    );

    let fresh = placement_req(
        request(&always, Some("s2"), "claude-sonnet-5", true, &[], 2_000),
        &["b1", "b2", "b3", "b4"],
        &[],
        false,
    );
    let SelectResult::Selected(sel) = select(&mut st, &always, &fresh, &mut rng()).0 else {
        panic!("a lands the fresh request by the strategy")
    };
    assert_eq!(
        sel.trace.placement, None,
        "the entries never confirmed a cache write, so none is live"
    );

    let when_cached = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        mode: AffinityMode::WhenCached,
        ..Pool::default()
    });
    let mut st2 = RoutingState::from_load(RoutingLoad::default());
    seed_affinity(
        &st2,
        vec![affinity_row(
            &when_cached,
            "claude-sonnet-5",
            "b1",
            AffinityChain::Message,
            "a",
            1_000,
            0,
            0,
            "root",
        )],
    );
    let portable_req = placement_req(
        request(
            &when_cached,
            Some("s1"),
            "claude-sonnet-5",
            true,
            &[],
            2_000,
        ),
        &["b1"],
        &[],
        false,
    );
    let (result, writes) = select(&mut st2, &when_cached, &portable_req, &mut rng());
    assert!(
        writes.is_empty(),
        "when_cached never pins a marker-less request"
    );
    let SelectResult::Selected(sel2) = result else {
        panic!("the strategy still lands it")
    };
    assert_eq!(sel2.pin, None, "no pin is read or created");
    assert_eq!(
        sel2.trace.placement, None,
        "the entry is never live, so the strategy lands it"
    );
}

/// A pin of session `session` on `account` as a load seeds it at `now`: created an hour ago, used a second ago,
/// refreshed a second ago under a five-minute TTL, 1 000 tokens of prompt, both averages 0 and folded at `now`.
fn seeded_pin(session: &str, account: &str, now: i64) -> PinRow {
    PinRow {
        key_id: "k".into(),
        session_id: session.into(),
        agent_id: "main".into(),
        model: "claude-sonnet-5".into(),
        account_id: account.into(),
        epoch: 1,
        created_at: now - HOUR,
        last_used_at: now - 1_000,
        key_generation: 0,
        incarnation: 1,
        burn: PinBurn::default(),
        burn_at: Some(now),
        burn_banded: PinBurn::default(),
        last_input_tokens: 1_000,
        last_refresh_at: now - 1_000,
        cache_ttl_ms: 300_000,
        shed_from: None,
        shed_at: None,
        static_hash: None,
        tail_hash: None,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
    }
}

/// `seeded_pin` burning `burn` input tokens a second in both copies, warm or cold: the default tables price it
/// at `burn`.
fn burning(session: &str, account: &str, burn: f64, warm: bool, now: i64) -> PinRow {
    let rates = input_rate(burn);
    PinRow {
        burn: rates,
        burn_banded: rates,
        last_refresh_at: if warm { now - 1_000 } else { now - HOUR },
        ..seeded_pin(session, account, now)
    }
}

fn input_rate(input: f64) -> PinBurn {
    PinBurn {
        input,
        ..PinBurn::default()
    }
}

/// A table pricing each listed model's input tokens at its rate (`None` for an input rate the table lacks) and
/// every other token at nothing.
fn input_prices(rows: &[(&str, Option<f64>)]) -> RateTable {
    let only_input = |input: Option<f64>| Rates {
        input,
        cache_read: Some(0.0),
        write_5m: Some(0.0),
        write_1h: Some(0.0),
        output: Some(0.0),
    };
    RateTable {
        rows: rows
            .iter()
            .map(|(model, input)| (model.to_string(), only_input(*input)))
            .collect(),
        fallback: only_input(Some(0.0)),
    }
}

fn loaded(pins: Vec<PinRow>) -> RoutingState {
    RoutingState::from_load(RoutingLoad {
        pins,
        ..RoutingLoad::default()
    })
}

#[test]
fn under_when_cached_a_marker_less_portable_request_neither_reads_nor_creates_a_pin() {
    let s = snapshot(Pool {
        mode: AffinityMode::WhenCached,
        // "a" sits behind the priority-0 tier, so the non-portable clause below only reaches it through the
        // pin: without `|| !request.portable`, the strategy would pick a priority-0 member instead.
        members: vec![("a", 1), ("b", 0), ("c", 0)],
        ..Pool::default()
    });
    let seeded = seeded_pin("s1", "a", 1_000);
    let mut st = loaded(vec![seeded.clone()]);
    let marker_less = SelectRequest {
        marker: false,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 2_000)
    };
    let (_, writes) = select(&mut st, &s, &marker_less, &mut rng());
    assert!(
        writes.is_empty(),
        "a marker-less request neither reads nor moves the pin"
    );
    assert_eq!(
        st.pin(&pin_key("s1")),
        Some(&seeded),
        "the pin lands by the strategy and is left exactly as it was"
    );

    let marked = SelectRequest {
        marker: true,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 3_000)
    };
    let (result, writes) = select(&mut st, &s, &marked, &mut rng());
    assert!(
        matches!(result, SelectResult::Selected(sel) if sel.pin.is_some()),
        "a marker on the request reads and renews the pin"
    );
    assert!(writes.iter().any(|w| matches!(w, StateWrite::UpsertPin(_))));

    let non_portable = SelectRequest {
        marker: false,
        ..request(&s, Some("s1"), "claude-sonnet-5", false, &[], 4_000)
    };
    let (result, _) = select(&mut st, &s, &non_portable, &mut rng());
    assert!(
        matches!(result, SelectResult::Selected(sel) if sel.account.id == "a"),
        "a non-portable request binds to the pin whatever affinity_mode says"
    );
}

#[test]
fn under_always_a_marker_less_request_pins_as_today() {
    let s = snapshot(Pool::default());
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let req = SelectRequest {
        marker: false,
        ..request(&s, Some("s1"), "claude-sonnet-5", true, &[], 1_000)
    };
    let (result, writes) = select(&mut st, &s, &req, &mut rng());
    assert!(matches!(result, SelectResult::Selected(sel) if sel.pin.is_some()));
    assert!(
        writes.iter().any(|w| matches!(w, StateWrite::UpsertPin(_))),
        "a marker-less request still pins under always"
    );
}

/// A turn of `session` whose class says nothing about its cache, so every rule reads the pin's stored warmth.
fn auxiliary(s: &RegistrySnapshot, session: &str, now: i64) -> SelectRequest {
    with_facts(
        request(s, Some(session), "claude-sonnet-5", true, &[], now),
        Some("quota"),
        false,
        None,
        &[],
    )
}

/// A main-conversation turn of `session`, which refreshes its pin.
fn main_turn(s: &RegistrySnapshot, session: &str, now: i64) -> SelectRequest {
    with_facts(
        request(s, Some(session), "claude-sonnet-5", true, &[], now),
        Some("main"),
        false,
        None,
        &[],
    )
}

fn landed(result: SelectResult) -> (String, Option<MigrateReason>) {
    match result {
        SelectResult::Selected(sel) => (sel.account.id, sel.migrated),
        other => panic!("expected a selection, got {other:?}"),
    }
}

fn marked(st: &RoutingState, session: &str, now: i64) -> bool {
    st.shed_marked(&pin_key(session), now, 900_000)
}

const MINUTE: i64 = 60_000;

#[test]
fn the_spent_toggle_gates_the_spent_move_and_a_destination_needs_a_reading_of_the_window_unless_it_is_an_api_key(
) {
    let pool = |migrate_when_spent: bool, kinds: Vec<(&'static str, AccountKind)>| Pool {
        members: vec![("a", 0), ("b", 1)],
        kinds,
        shedding: Shedding {
            migrate_when_spent,
            ..Shedding::default()
        },
        ..Pool::default()
    };
    let spent_a = |st: &mut RoutingState| {
        st.record_sample("a", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    };
    let s = snapshot(pool(true, vec![]));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    spent_a(&mut st);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("a".into(), None),
        "a sibling never read holds no reading of the window, so it is no destination and the upstream decides"
    );
    st.record_sample(
        "b",
        refreshing(0.2, T + 9 * HOUR, T + 9 * HOUR, T + 1_000 - HOUR),
    );
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 2_000),
                &mut rng()
            )
            .0
        ),
        ("b".into(), Some(MigrateReason::Spent)),
        "a sibling read an hour ago, its reset ahead, is a destination"
    );

    let off = snapshot(pool(false, vec![]));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &off,
        &request(&off, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    spent_a(&mut st);
    st.record_sample("b", sample(0.2, 0.2, T + 500));
    assert_eq!(
        landed(
            select(
                &mut st,
                &off,
                &request(&off, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("a".into(), None),
        "with the toggle off a spent pin stays and the upstream's answer decides"
    );

    let keyed = snapshot(pool(true, vec![("b", AccountKind::ApiKey)]));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &keyed,
        &request(&keyed, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    spent_a(&mut st);
    assert_eq!(
        landed(
            select(
                &mut st,
                &keyed,
                &request(&keyed, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("b".into(), Some(MigrateReason::Spent)),
        "an api_key sibling holds allowance without a sample"
    );
}

#[test]
fn a_spent_sibling_whose_reading_passed_its_reset_holds_allowance_and_the_shed_trigger_reads_zero_after_a_reset(
) {
    let s = snapshot(Pool {
        select_below: Some(0.8),
        ..shedding_pool(Shedding::default())
    });
    let mut st = loaded(vec![burning("s1", "b", 4.0, false, T)]);
    // a read its 5-hour window full an hour ago, and that window reset a minute ago; b is spent until its reset.
    st.record_sample("a", refreshing(1.0, T - MINUTE, T + 9 * HOUR, T - HOUR));
    st.record_sample("b", refreshing(1.0, T + HOUR, T + 9 * HOUR, T - 1_000));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), Some(MigrateReason::Spent)),
        "a's full reading passed its reset, so a holds allowance and takes the spent pin"
    );
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng()).0),
        ("a".into(), None)
    );
    assert_eq!(
        st.last_shed("a"),
        None,
        "the shed trigger reads zero after the reset, so no round ran on a"
    );
    st.record_sample("b", refreshing(0.1, T + HOUR, T + 9 * HOUR, T + 1_500));
    assert_eq!(
        picked(
            &select(
                &mut st,
                &s,
                &request(&s, Some("s2"), "claude-sonnet-5", true, &[], T + 2_000),
                &mut rng()
            )
            .0
        ),
        "a",
        "the ceiling reads zero after the reset too, so a stays under it"
    );
}

#[test]
fn the_overage_move_waits_out_the_grace_and_lands_on_plan_funded_siblings_only() {
    let pool = |grace: u32| Pool {
        members: vec![("a", 0), ("b", 1), ("c", 1)],
        kinds: vec![("c", AccountKind::ApiKey)],
        failover: Failover {
            overage_move_grace_minutes: grace,
            ..Failover::default()
        },
        ..Pool::default()
    };
    let s = snapshot(pool(30));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    let at = |s: &RegistrySnapshot, session: &str, now: i64| {
        request(s, Some(session), "claude-sonnet-5", true, &[], now)
    };
    select(&mut st, &s, &at(&s, "s1", T), &mut rng());
    st.record_overage("a", overage(true, T + 500, Some(T + 10 * MINUTE)));
    assert_eq!(
        landed(select(&mut st, &s, &at(&s, "s1", T + 1_000), &mut rng()).0),
        ("a".into(), None),
        "the plan returns within the 30-minute grace, so the session keeps its cache"
    );
    st.record_overage("a", overage(true, T + 1_500, Some(T + 40 * MINUTE)));
    assert_eq!(
        landed(select(&mut st, &s, &at(&s, "s1", T + 2_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Overage)),
        "past the grace the pin moves to the plan-funded sibling"
    );
    select(&mut st, &s, &at(&s, "s2", T + 3_000), &mut rng());
    assert_eq!(pinned(&st, "s2").account_id, "a");
    st.record_cooldown(cooled("b", "account", T + HOUR, "five_hour", "rate_limit"));
    assert_eq!(
        landed(select(&mut st, &s, &at(&s, "s2", T + 4_000), &mut rng()).0),
        ("a".into(), None),
        "with b cooled the only sibling left is the api_key one, which is no plan-funded destination"
    );

    let now_s = snapshot(pool(0));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(&mut st, &now_s, &at(&now_s, "s1", T), &mut rng());
    st.record_overage("a", overage(true, T + 500, Some(T + 10 * MINUTE)));
    assert_eq!(
        landed(select(&mut st, &now_s, &at(&now_s, "s1", T + 1_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Overage)),
        "a grace of 0 moves the pin at once"
    );
}

#[test]
fn the_overage_move_lands_only_on_a_sibling_holding_allowance() {
    // b outranks c, so a destination set that kept the spent b would land the pin there.
    let placed = |members: Vec<(&'static str, u32)>| {
        let s = snapshot(Pool {
            members,
            ..Pool::default()
        });
        let mut st = RoutingState::from_load(RoutingLoad::default());
        select(
            &mut st,
            &s,
            &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
            &mut rng(),
        );
        st.record_overage("a", overage(true, T + 500, Some(T + 40 * MINUTE)));
        st.record_sample("b", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
        st.record_sample("c", sample(0.2, 0.2, T + 500));
        (s, st)
    };
    let (s, mut st) = placed(vec![("a", 0), ("b", 1), ("c", 2)]);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("c".into(), Some(MigrateReason::Overage)),
        "b is out of overage but spent, which is no plan to return to"
    );
    let (s, mut st) = placed(vec![("a", 0), ("b", 1)]);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("a".into(), None),
        "with only the spent sibling the pin renews in place"
    );
}

#[test]
fn the_overage_move_takes_a_warm_pin_and_a_cold_one_alike_and_names_overage() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let dwell_ago = |pin: PinRow| PinRow {
        created_at: T - 900_000,
        ..pin
    };
    for (reset, expected) in [
        (10 * MINUTE, ("a".to_string(), None)),
        (40 * MINUTE, ("b".to_string(), Some(MigrateReason::Overage))),
    ] {
        let mut st = loaded(vec![
            dwell_ago(burning("cold", "a", 0.0, false, T)),
            dwell_ago(burning("warm", "a", 0.0, true, T)),
        ]);
        st.record_overage("a", overage(true, T - 500, Some(T + reset)));
        assert_eq!(
            landed(select(&mut st, &s, &auxiliary(&s, "cold", T), &mut rng()).0),
            expected,
            "the cold pin, reset {reset} ms away"
        );
        assert_eq!(
            landed(select(&mut st, &s, &main_turn(&s, "warm", T), &mut rng()).0),
            expected,
            "the warm pin, reset {reset} ms away"
        );
    }
}

#[test]
fn a_cold_pin_on_an_api_key_member_returns_to_plan_and_a_warm_one_waits_out_the_dwell() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        kinds: vec![("a", AccountKind::ApiKey)],
        ..Pool::default()
    });
    let mut st = loaded(vec![
        PinRow {
            created_at: T - 5 * MINUTE,
            ..burning("warm", "a", 0.0, true, T)
        },
        PinRow {
            created_at: T - MINUTE,
            ..burning("cold", "a", 0.0, false, T)
        },
    ]);
    // b's own window resets soon; the return names no grace, so nothing about it is read.
    st.record_sample("b", refreshing(0.3, T + 10 * MINUTE, T + 9 * HOUR, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm", T), &mut rng()).0),
        ("a".into(), None),
        "a warm pin five minutes on the member waits out the dwell"
    );
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold", T), &mut rng()).0),
        ("b".into(), Some(MigrateReason::ReturnToPlan)),
        "a cold pin returns to plan at once"
    );
    let dwell_done = T - 5 * MINUTE + 900_000;
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm", dwell_done), &mut rng()).0),
        ("b".into(), Some(MigrateReason::ReturnToPlan)),
        "a dwell after it landed on the member the warm pin returns too"
    );
}

#[test]
fn a_restart_on_a_warm_pin_is_cold_for_the_return_rule_on_that_request() {
    let s = snapshot(Pool {
        members: vec![("a", 0), ("b", 1)],
        kinds: vec![("a", AccountKind::ApiKey)],
        ..Pool::default()
    });
    let mut st = loaded(vec![PinRow {
        created_at: T - MINUTE,
        static_hash: Some("s0".into()),
        tail_hash: Some("m2".into()),
        ..burning("s1", "a", 0.0, true, T)
    }]);
    assert!(warm(&pinned(&st, "s1"), T), "the stored pin is warm");
    let restart = with_facts(
        request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        None,
        false,
        Some("s0"),
        &["x1"],
    );
    assert_eq!(
        landed(select(&mut st, &s, &restart, &mut rng()).0),
        ("b".into(), Some(MigrateReason::ReturnToPlan)),
        "the restart is cold on the request that carries it, not one request later"
    );
}

#[test]
fn a_cold_pin_on_a_winding_down_account_drains_and_a_warm_one_stays() {
    let s = snapshot_with_terms(
        Pool {
            members: vec![("a", 0), ("b", 1)],
            ..Pool::default()
        },
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(T + 10 * MINUTE),
            tier: None,
        }],
    );
    let mut st = loaded(vec![
        burning("cold", "a", 0.0, false, T),
        burning("warm", "a", 0.0, true, T),
    ]);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold", T), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Drain)),
        "a cold pin leaves the account winding down"
    );
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm", T), &mut rng()).0),
        ("a".into(), None),
        "a warm pin stays until the end refuses it"
    );
}

/// A pool whose moves read a threshold of 0.8, where `b` outranks `c`: without the cool preference every move
/// below would land on `b`.
fn cool_preference_pool(kinds: Vec<(&'static str, AccountKind)>) -> Pool {
    Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1), ("c", 2)],
        kinds,
        ..Pool::default()
    }
}

/// `b` reads above the threshold and still holds allowance, so every rule would take it; `c` reads cool.
fn hot_b_cool_c(st: &mut RoutingState, at: i64) {
    st.record_sample("b", sample(0.9, 0.3, at));
    st.record_sample("c", sample(0.1, 0.1, at));
}

#[test]
fn a_spent_move_prefers_a_cool_sibling() {
    let s = snapshot(cool_preference_pool(vec![]));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    st.record_sample("a", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    hot_b_cool_c(&mut st, T + 500);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("c".into(), Some(MigrateReason::Spent)),
        "the cool sibling takes the spent pin over the hot one that outranks it"
    );
}

#[test]
fn an_overage_move_prefers_a_cool_sibling() {
    let s = snapshot(cool_preference_pool(vec![]));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    st.record_overage("a", overage(true, T + 500, Some(T + 40 * MINUTE)));
    hot_b_cool_c(&mut st, T + 500);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("c".into(), Some(MigrateReason::Overage)),
        "the cool sibling takes the pin back to plan over the hot one that outranks it"
    );
}

#[test]
fn a_return_to_plan_move_prefers_a_cool_sibling() {
    let s = snapshot(cool_preference_pool(vec![("a", AccountKind::ApiKey)]));
    let mut st = loaded(vec![PinRow {
        created_at: T - MINUTE,
        ..burning("cold", "a", 0.0, false, T)
    }]);
    hot_b_cool_c(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold", T), &mut rng()).0),
        ("c".into(), Some(MigrateReason::ReturnToPlan)),
        "the cool sibling takes the cold pin off the key over the hot one that outranks it"
    );
}

#[test]
fn a_drain_move_prefers_a_cool_sibling() {
    let s = snapshot_with_terms(
        cool_preference_pool(vec![]),
        vec![SubscriptionTerm {
            account_id: "a".into(),
            since: Some(0),
            trial_ends_at: None,
            cancelled: true,
            ends_at: Some(T + 10 * MINUTE),
            tier: None,
        }],
    );
    let mut st = loaded(vec![burning("cold", "a", 0.0, false, T)]);
    hot_b_cool_c(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold", T), &mut rng()).0),
        ("c".into(), Some(MigrateReason::Drain)),
        "the cool sibling takes the cold pin off the ending account over the hot one that outranks it"
    );
}

#[test]
fn a_spent_move_lands_on_a_hot_sibling_when_no_cool_one_exists() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        ..Pool::default()
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    st.record_sample("a", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    st.record_sample("b", sample(0.9, 0.3, T + 500));
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("b".into(), Some(MigrateReason::Spent)),
        "the preference falls through to the rule's own set rather than leaving the pin on a spent account"
    );
}

#[test]
fn a_hot_api_key_sibling_is_not_preferred() {
    let s = snapshot(cool_preference_pool(vec![("b", AccountKind::ApiKey)]));
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    st.record_sample("a", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    // b reads no plan window at all; its worst live bucket is what says it is hot.
    st.record_sample(
        "b",
        UtilizationSample {
            observed_at: T + 500,
            ..Default::default()
        }
        .with(
            "itpm:claude-sonnet-5",
            WindowReading {
                utilization: 0.9,
                reset: Some(T + MINUTE),
                observed_at: T + 500,
            },
        ),
    );
    st.record_sample("c", sample(0.1, 0.1, T + 500));
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("c".into(), Some(MigrateReason::Spent)),
        "the api_key sibling's bucket reads above the band, so the cool one is preferred over it"
    );
}

#[test]
fn a_cool_sibling_at_its_cap_is_passed_over_for_a_hot_one_under_it() {
    let s = snapshot(Pool {
        failover: Failover {
            in_flight_cap: 1,
            ..Failover::default()
        },
        ..cool_preference_pool(vec![])
    });
    let mut st = RoutingState::from_load(RoutingLoad::default());
    select(
        &mut st,
        &s,
        &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T),
        &mut rng(),
    );
    st.record_sample("a", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    hot_b_cool_c(&mut st, T + 500);
    let on_c = st.in_flight().dispatched("c", T + 500);
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &request(&s, Some("s1"), "claude-sonnet-5", true, &[], T + 1_000),
                &mut rng()
            )
            .0
        ),
        ("b".into(), Some(MigrateReason::Spent)),
        "c is cool but has no room, so the move falls through to b rather than being dropped at the cap"
    );
    drop(on_c);
}

#[test]
fn a_marked_pin_on_a_cooled_account_is_released_with_cooldown_rather_than_shed() {
    let s = snapshot(shedding_pool(Shedding::default()));
    // Two cold pins, so the round has one to spare: it marks the hotter.
    let mut st = loaded(vec![
        burning("s1", "a", 4.0, false, T),
        burning("s0", "a", 1.0, false, T),
    ]);
    hot_a_cool_b(&mut st, T - 500);
    st.record_cooldown(cooled(
        "a",
        "model:claude-sonnet-5",
        T + 10 * MINUTE,
        "retry_after",
        "rate_limit",
    ));
    let req = auxiliary(&s, "s1", T);
    let reasons = release_reasons(&st, &s, &pinned(&st, "s1"), &req);
    let (result, _) = select(&mut st, &s, &req, &mut rng());
    let SelectResult::Selected(sel) = result else {
        panic!("expected a selection, got {result:?}")
    };
    assert_eq!(
        (
            sel.account.id.as_str(),
            sel.migrated,
            sel.trace.moved,
            sel.trace.renewed
        ),
        ("b", None, None, false),
        "the cooled account's pin is released and the session placed afresh, not shed"
    );
    assert!(
        reasons.contains(&"cooldown"),
        "the release names the cooldown: {reasons:?}"
    );
    assert!(
        st.last_shed("a").is_some(),
        "the round still ran, since the cooldown is the model's alone"
    );
    assert!(!marked(&st, "s1", T), "the mark left with the released pin");

    let cleared = T + 10 * MINUTE + 1_000;
    let (result, _) = select(&mut st, &s, &auxiliary(&s, "s1", cleared), &mut rng());
    let SelectResult::Selected(sel) = result else {
        panic!("expected a selection, got {result:?}")
    };
    assert_eq!(
        (sel.account.id.as_str(), sel.trace.renewed, sel.trace.moved),
        ("b", true, None),
        "with the cooldown cleared the session's next request renews in place"
    );
}

/// A pool whose round runs on `a` at 0.8, with `b` a cool destination.
fn shedding_pool(shedding: Shedding) -> Pool {
    Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1)],
        shedding,
        ..Pool::default()
    }
}

fn hot_a_cool_b(st: &mut RoutingState, at: i64) {
    st.record_sample("a", sample(0.9, 0.3, at));
    st.record_sample("b", sample(0.1, 0.1, at));
}

/// A margin of 0.2 over pace on the shedding pool, where the cool line sits at 0.15.
fn pace_pool() -> Pool {
    Pool {
        migrate: Some(0.2),
        ..shedding_pool(Shedding::default())
    }
}

/// Two cold pins on `a`, so a round there marks `s1`, the heavier, and leaves `s0`.
fn two_pins_on_a() -> RoutingState {
    loaded(vec![
        burning("s1", "a", 4.0, false, T),
        burning("s0", "a", 1.0, false, T),
    ])
}

/// A 5-hour reading with no reset, so its lead is its utilization, and a 7-day reading whose reset sits
/// `remaining_tenths` tenths of a week after `T`, so the rest of the week has elapsed.
fn weekly(
    five_hour: f64,
    seven_day: f64,
    remaining_tenths: i64,
    observed_at: i64,
) -> UtilizationSample {
    sample(five_hour, seven_day, observed_at).with(
        SEVEN_DAY,
        WindowReading {
            utilization: seven_day,
            reset: Some(T + WEEK_MS * remaining_tenths / 10),
            observed_at,
        },
    )
}

#[test]
fn a_pin_half_through_a_week_ten_percent_elapsed_sheds_at_a_margin_of_a_fifth() {
    let s = snapshot(pace_pool());
    let mut st = two_pins_on_a();
    // 0.5 with a tenth of the week gone runs 0.4 ahead of pace; the 5-hour window leads by 0.1.
    st.record_sample("a", weekly(0.1, 0.5, 9, T - 500));
    // 0.3 with six tenths gone runs 0.3 behind pace, so b is cool on both windows though it reads over 0.15.
    st.record_sample("b", weekly(0.1, 0.3, 4, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "the week's lead of 0.4 starts a round and the marked pin moves to the sibling behind its pace"
    );
    assert_eq!(st.last_shed("a"), Some(T));
}

#[test]
fn the_same_reading_sixty_percent_elapsed_does_not_shed() {
    let s = snapshot(pace_pool());
    let mut st = two_pins_on_a();
    // 0.5 with six tenths of the week gone runs 0.1 behind pace; the 5-hour window leads by 0.1.
    st.record_sample("a", weekly(0.1, 0.5, 4, T - 500));
    st.record_sample("b", sample(0.1, 0.1, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), None),
        "a week behind its pace runs no round, so the pin renews in place"
    );
    assert_eq!(st.last_shed("a"), None);
}

#[test]
fn a_five_hour_window_at_nine_tenths_eighty_percent_elapsed_does_not_shed() {
    let s = snapshot(pace_pool());
    let mut st = two_pins_on_a();
    // An hour left of five puts 0.9 at 0.1 ahead of pace; the week reads 0.1 with no reset, 0.1 ahead.
    st.record_sample(
        "a",
        sample(0.9, 0.1, T - 500).with(
            FIVE_HOUR,
            WindowReading {
                utilization: 0.9,
                reset: Some(T + HOUR),
                observed_at: T - 500,
            },
        ),
    );
    st.record_sample("b", sample(0.1, 0.1, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), None),
        "a 5-hour lead of 0.1 sits under the margin of 0.2"
    );
    assert_eq!(st.last_shed("a"), None);
}

#[test]
fn a_destination_at_four_tenths_twenty_percent_elapsed_is_not_cool_and_one_at_three_tenths_is() {
    let s = snapshot(pace_pool());
    let mut st = two_pins_on_a();
    // The 5-hour window with no reset leads by its whole 0.9, so a round runs on a and marks s1.
    st.record_sample("a", sample(0.9, 0.1, T - 500));
    // 0.4 with two tenths of the week gone runs 0.2 ahead, at or over the cool line of 0.15.
    st.record_sample("b", weekly(0.1, 0.4, 8, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), None),
        "b's week leads by 0.2, so b is no destination and the mark stands"
    );
    // 0.3 at the same point of the week runs 0.1 ahead, under the line.
    st.record_sample("b", weekly(0.1, 0.3, 8, T + 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "b's week leads by 0.1, so b is cool and takes the marked pin"
    );
}

#[test]
fn a_subscription_pin_in_overage_orders_the_round_by_the_dollar_average_and_leaving_overage_reads_the_window_share_back(
) {
    let s = snapshot(Pool {
        policy: OveragePolicy::Allow,
        ..shedding_pool(Shedding::default())
    });
    // Both pins burn the same tokens; the dollar table weighs Sonnet's nine times Opus's and the 5-hour share
    // table the other way round, so the unit alone decides which pin is heavier.
    let opus = PinRow {
        model: "claude-opus-5".into(),
        ..burning("opus", "a", 1.0, false, T)
    };
    let sonnet = burning("sonnet", "a", 1.0, false, T);
    let mut st = loaded(vec![opus, sonnet]);
    st.set_rates(BurnRates {
        dollars: input_prices(&[("claude-opus-5", Some(1.0)), ("claude-sonnet-5", Some(9.0))]),
        five_hour: input_prices(&[("claude-opus-5", Some(9.0)), ("claude-sonnet-5", Some(1.0))]),
        fast: RateTable::unit(),
    });
    let opus_key = PinKey {
        model: "claude-opus-5".into(),
        ..pin_key("opus")
    };
    let opus_turn = |now: i64| {
        with_facts(
            request(&s, Some("opus"), "claude-opus-5", true, &[], now),
            Some("quota"),
            false,
            None,
            &[],
        )
    };
    hot_a_cool_b(&mut st, T - 500);
    st.record_overage("a", overage(true, T - 500, Some(T + 10 * MINUTE)));
    assert_eq!(
        landed(select(&mut st, &s, &opus_turn(T), &mut rng()).0),
        ("a".into(), None),
        "in overage the round reads dollars, so the Opus pin is the last unmarked one"
    );
    assert!(marked(&st, "sonnet", T) && !st.shed_marked(&opus_key, T, 900_000));

    let later = T + 900_000;
    hot_a_cool_b(&mut st, later - 500);
    assert!(
        !st.in_overage("a", later, FALLBACK_MS),
        "the reset has passed"
    );
    assert_eq!(
        landed(select(&mut st, &s, &opus_turn(later), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "back on plan the round reads the 5-hour share and marks the Opus pin first"
    );
    let untouched = pinned(&st, "sonnet");
    assert_eq!(
        (untouched.burn, untouched.burn_banded),
        (input_rate(1.0), input_rate(1.0)),
        "neither copy was reset by the switch"
    );
}

#[test]
fn the_shedding_round_orders_cold_pins_first_then_warm_by_burn_over_rewrite_and_stops_at_the_share()
{
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(vec![
        burning("warm-4", "a", 4.0, true, T),
        burning("cold-3", "a", 3.0, false, T),
        burning("warm-2", "a", 2.0, true, T),
        burning("warm-1", "a", 1.0, true, T),
    ]);
    // Sonnet's measured-scale share per token: one table's rate scales every pin of the account alike, so the
    // order and the share of the sum read as the token counts would.
    st.set_rates(BurnRates {
        five_hour: input_prices(&[("claude-sonnet-5", Some(0.0454e-6))]),
        ..BurnRates::default()
    });
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng()).0),
        ("a".into(), None),
        "the round ran on warm-1's request and did not mark it"
    );
    assert_eq!(st.last_shed("a"), Some(T));
    let marks: Vec<&str> = ["warm-4", "cold-3", "warm-2", "warm-1"]
        .into_iter()
        .filter(|session| marked(&st, session, T))
        .collect();
    assert_eq!(
        marks,
        ["warm-4", "cold-3"],
        "cold-3 first, then warm-4, and 7 of 10 reaches half"
    );
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold-3", T + 1_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed))
    );
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-4", T + 2_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "a marked warm pin moves at its next request"
    );
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-2", T + 3_000), &mut rng()).0),
        ("a".into(), None),
        "an unmarked pin renews in place"
    );
    assert_eq!(
        st.last_shed("a"),
        Some(T),
        "a second request inside the dwell runs no new round, so warm-2 is still unmarked"
    );
}

#[test]
fn an_unpriced_pin_is_never_marked_and_stays_out_of_the_shed_share_sum() {
    let s = snapshot(shedding_pool(Shedding::default()));
    let guess = |session: &str, warm: bool| PinRow {
        model: "claude-guess".into(),
        ..burning(session, "a", 9.0, warm, T)
    };
    let mut st = loaded(vec![
        guess("cold-guess", false),
        burning("cold-3", "a", 3.0, false, T),
        guess("warm-guess", true),
        burning("warm-4", "a", 4.0, true, T),
        burning("warm-1", "a", 1.0, true, T),
    ]);
    // The guess model's input rate is unknown, so its two pins burn nine tokens a second that price to nothing.
    st.set_rates(BurnRates {
        five_hour: input_prices(&[("claude-sonnet-5", Some(1.0)), ("claude-guess", None)]),
        ..BurnRates::default()
    });
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng()).0),
        ("a".into(), None)
    );
    let marks: Vec<&str> = ["cold-guess", "cold-3", "warm-guess", "warm-4", "warm-1"]
        .into_iter()
        .filter(|session| marked(&st, session, T))
        .collect();
    assert_eq!(
        marks,
        ["cold-3", "warm-4"],
        "the priced burn is 8 and the target 4: cold-3 then warm-4 reach it, and neither guess is marked"
    );
}

/// The share table the router builds from the cost table prices a model it does not list at no rate, so a pin on
/// such a model reads no burn, adds nothing to the account's sum and is never marked beside a priced pin.
#[test]
fn a_pin_on_a_model_the_share_table_does_not_list_burns_nothing_and_is_never_marked() {
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(vec![
        PinRow {
            model: "claude-opus-4-8".into(),
            ..burning("cold-unlisted", "a", 9.0, false, T)
        },
        burning("cold-3", "a", 3.0, false, T),
        burning("warm-1", "a", 1.0, true, T),
    ]);
    let none = Rates {
        input: None,
        cache_read: None,
        write_5m: None,
        write_1h: None,
        output: None,
    };
    st.set_rates(BurnRates {
        five_hour: RateTable {
            fallback: none,
            ..input_prices(&[("claude-sonnet-5", Some(1.0))])
        },
        ..BurnRates::default()
    });
    hot_a_cool_b(&mut st, T - 500);
    let (pins, sum) = st.burn_of("a", BurnUnit::WindowShare, st.rates(), T, HOUR);
    let unlisted = pins
        .iter()
        .find(|(p, _)| p.model == "claude-opus-4-8")
        .map(|(_, burn)| *burn);
    assert_eq!(
        unlisted,
        Some(None),
        "the unlisted pin is one of the account's pins and reads no burn"
    );
    assert_eq!(
        sum,
        pins.iter().filter_map(|(_, burn)| *burn).sum::<f64>(),
        "the sum is the priced pins' alone"
    );
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng()).0),
        ("a".into(), None)
    );
    assert!(
        marked(&st, "cold-3", T) && !marked(&st, "cold-unlisted", T),
        "the priced pin is marked, the guess never"
    );
}

#[test]
fn a_move_consumes_the_mark_so_a_pin_returning_inside_the_dwell_renews_in_place() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1), ("c", 2)],
        ..Pool::default()
    });
    let mut st = loaded(vec![
        burning("s1", "a", 4.0, false, T),
        burning("s0", "a", 1.0, false, T),
    ]);
    // c holds no reading, so it reads as cool and a mark that outlived the move would find it a destination,
    // while the spent move below, which needs a reading of the window, has only a to go to.
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "a's round marks s1 and it moves"
    );
    st.record_sample("a", sample(0.9, 0.3, T + 500));
    st.record_sample("b", refreshing(1.0, T + 9 * HOUR, T + 9 * HOUR, T + 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng()).0),
        ("a".into(), Some(MigrateReason::Spent)),
        "b spends its window and the spent move takes s1 back to a, inside a's dwell"
    );
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 2_000), &mut rng()).0),
        ("a".into(), None),
        "a is still hot and its round still stands, but the move consumed s1's mark"
    );
    assert_eq!(st.last_shed("a"), Some(T), "no new round ran on a");
}

#[test]
fn a_pin_that_fails_over_off_its_account_spends_its_mark_there() {
    let s = snapshot(Pool {
        migrate: Some(0.8),
        members: vec![("a", 0), ("b", 1), ("c", 2)],
        ..Pool::default()
    });
    let mut st = loaded(vec![
        burning("s1", "a", 4.0, false, T),
        burning("s0", "a", 1.0, false, T),
    ]);
    // No sibling is cool, so a's round marks s1 and it stays.
    st.record_sample("a", sample(0.9, 0.3, T - 500));
    st.record_sample("b", sample(0.9, 0.3, T - 500));
    st.record_sample("c", sample(0.9, 0.3, T - 500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T), &mut rng()).0),
        ("a".into(), None)
    );
    assert!(marked(&st, "s1", T));
    // What the task does with a cooldown report that names the pin: the row, then the invalidation.
    let fail_over = |st: &mut RoutingState, account: &str, until: i64| {
        let current = pinned(st, "s1");
        st.record_cooldown(cooled(account, "account", until, "five_hour", "rate_limit"));
        assert!(st.invalidate_pin(
            &PinRef {
                key_id: "k".into(),
                session_id: "s1".into(),
                agent_id: "main".into(),
                model: "claude-sonnet-5".into(),
                epoch: current.epoch,
                lane: current.lane,
            },
            account,
        ));
    };
    fail_over(&mut st, "a", T + 5_000);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 1_000), &mut rng()).0),
        ("b".into(), None),
        "the session fails over off the cooled a"
    );
    fail_over(&mut st, "b", T + HOUR);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 6_000), &mut rng()).0),
        ("a".into(), None),
        "a's cooldown has ended, so the session pins to a again inside a's dwell"
    );
    // c turns cool, so a mark that outlived the fail-over would find a destination.
    st.record_sample("c", sample(0.1, 0.1, T + 6_500));
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "s1", T + 7_000), &mut rng()).0),
        ("a".into(), None),
        "a is still hot and its round still stands, but leaving a spent s1's mark"
    );
    assert_eq!(st.last_shed("a"), Some(T), "no new round ran on a");
}

#[test]
fn a_round_never_marks_the_last_pin_and_a_zero_target_marks_nothing() {
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(vec![burning("only", "a", 100.0, false, T)]);
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "only", T), &mut rng()).0),
        ("a".into(), None),
        "a one-pin account sheds nothing"
    );

    let mut st = loaded(vec![
        burning("cold-1", "a", 1.0, false, T),
        burning("warm-9", "a", 9.0, true, T),
    ]);
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold-1", T), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "the cold pin is marked first and moves"
    );
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-9", T + 1_000), &mut rng()).0),
        ("a".into(), None),
        "the warm pin was the last unmarked one, whatever the target still asked"
    );

    let four = |burns: [f64; 4]| {
        let mut st = loaded(
            ["cold-a", "cold-b", "cold-c", "cold-d"]
                .into_iter()
                .zip(burns)
                .map(|(session, burn)| burning(session, "a", burn, false, T))
                .collect(),
        );
        hot_a_cool_b(&mut st, T - 500);
        st
    };
    let mut st = four([0.0; 4]);
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold-a", T), &mut rng()).0),
        ("a".into(), None),
        "no burn in the window is a zero target"
    );
    assert!(["cold-a", "cold-b", "cold-c", "cold-d"]
        .iter()
        .all(|session| !marked(&st, session, T)));

    let unshared = snapshot(shedding_pool(Shedding {
        shed_share: 0.0,
        ..Shedding::default()
    }));
    let mut st = four([4.0, 3.0, 2.0, 1.0]);
    assert_eq!(
        landed(
            select(
                &mut st,
                &unshared,
                &auxiliary(&unshared, "cold-a", T),
                &mut rng()
            )
            .0
        ),
        ("a".into(), None),
        "a share of 0 marks nothing"
    );
    assert!(["cold-a", "cold-b", "cold-c", "cold-d"]
        .iter()
        .all(|session| !marked(&st, session, T)));
}

#[test]
fn warm_sessions_never_stops_after_the_cold_pins_and_always_ignores_warmth() {
    // The warm pins' burns over their prompt size: warm-2 0.2, warm-4 0.04, warm-1 0.001.
    let pins = || {
        vec![
            burning("cold-3", "a", 3.0, false, T),
            PinRow {
                last_input_tokens: 100,
                ..burning("warm-4", "a", 4.0, true, T)
            },
            PinRow {
                last_input_tokens: 10,
                ..burning("warm-2", "a", 2.0, true, T)
            },
            burning("warm-1", "a", 1.0, true, T),
        ]
    };
    for (knob, expected) in [
        (WarmSessions::WhenNeeded, vec!["cold-3", "warm-2"]),
        (WarmSessions::Never, vec!["cold-3"]),
        (WarmSessions::Always, vec!["warm-4", "warm-2"]),
    ] {
        let s = snapshot(shedding_pool(Shedding {
            warm_sessions: knob,
            ..Shedding::default()
        }));
        let mut st = loaded(pins());
        hot_a_cool_b(&mut st, T - 500);
        select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng());
        let marks: Vec<&str> = ["cold-3", "warm-4", "warm-2", "warm-1"]
            .into_iter()
            .filter(|session| marked(&st, session, T))
            .collect();
        assert_eq!(marks, expected, "{knob:?}");
    }
}

/// An unpriced cold pin takes no mark, so the round's order reaches a warm pin while the cold group's marks still
/// fall short of its length; `never` keeps that warm pin unmarked all the same.
#[test]
fn warm_sessions_never_marks_no_warm_pin_when_an_unpriced_cold_pin_leaves_the_cold_marks_short() {
    let s = snapshot(shedding_pool(Shedding {
        warm_sessions: WarmSessions::Never,
        ..Shedding::default()
    }));
    let mut st = loaded(vec![
        burning("cold-1", "a", 1.0, false, T),
        PinRow {
            model: "claude-guess".into(),
            ..burning("cold-guess", "a", 9.0, false, T)
        },
        burning("warm-4", "a", 4.0, true, T),
        burning("warm-1", "a", 1.0, true, T),
    ]);
    st.set_rates(BurnRates {
        five_hour: input_prices(&[("claude-sonnet-5", Some(1.0)), ("claude-guess", None)]),
        ..BurnRates::default()
    });
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng()).0),
        ("a".into(), None)
    );
    let marks: Vec<&str> = ["cold-1", "warm-4", "warm-1"]
        .into_iter()
        .filter(|session| marked(&st, session, T))
        .collect();
    assert_eq!(
        marks,
        ["cold-1"],
        "the priced burn is 6 and the target 3: cold-1 is marked, the order passes the unpriced cold pin and reaches \
         warm-4, which `never` keeps"
    );
    let guess = PinKey {
        model: "claude-guess".into(),
        ..pin_key("cold-guess")
    };
    assert!(
        !st.shed_marked(&guess, T, 900_000),
        "the unpriced cold pin is never marked"
    );
}

#[test]
fn a_shed_pin_never_returns_to_the_account_that_shed_it_within_the_dwell_even_when_that_account_cooled(
) {
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(vec![
        burning("p4", "a", 4.0, false, T),
        burning("p3", "a", 3.0, false, T),
        burning("p2", "a", 2.0, false, T),
        burning("p1", "a", 1.0, false, T),
    ]);
    hot_a_cool_b(&mut st, T - 500);
    for (session, now) in [("p4", T), ("p3", T + 1_000)] {
        assert_eq!(
            landed(select(&mut st, &s, &auxiliary(&s, session, now), &mut rng()).0),
            ("b".into(), Some(MigrateReason::Shed)),
            "{session}: A sheds its two hottest pins to B"
        );
        let moved = pinned(&st, session);
        assert_eq!(
            (moved.shed_from.as_deref(), moved.shed_at),
            (Some("a"), Some(now)),
            "{session}: the moved pin remembers where it was shed from"
        );
    }
    // The two sessions burn on B from here. The state is rebuilt from the pins, as a restart leaves it: A's own
    // round is forgotten with the marks, so only each pin's `shed_from` keeps A out.
    let at = T + 2_000;
    let pins = st
        .pins()
        .into_iter()
        .map(|p| match p.session_id.as_str() {
            "p4" => PinRow {
                burn: input_rate(5.0),
                burn_banded: input_rate(5.0),
                burn_at: Some(at),
                ..p
            },
            "p3" => PinRow {
                burn: input_rate(1.0),
                burn_banded: input_rate(1.0),
                burn_at: Some(at),
                ..p
            },
            _ => p,
        })
        .collect();
    let mut st = loaded(pins);
    st.record_sample("a", sample(0.1, 0.1, at));
    st.record_sample("b", sample(0.9, 0.3, at));
    let crossed = T + 3_000;
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "p4", crossed), &mut rng()).0),
        ("b".into(), None),
        "B's round marks p4, and A, cool now, is still no destination for a pin it shed"
    );
    assert!(marked(&st, "p4", crossed));
    let after = T + 900_001;
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "p4", after), &mut rng()).0),
        ("a".into(), Some(MigrateReason::Shed)),
        "past the dwell the pin may move back"
    );
}

#[test]
fn a_compacted_request_on_a_warm_pin_is_first_in_the_order_on_that_request() {
    // A small share: the round marks exactly the first pin in its order.
    let s = snapshot(shedding_pool(Shedding {
        shed_share: 0.1,
        ..Shedding::default()
    }));
    let pins = || {
        vec![
            burning("warm-9", "a", 9.0, true, T),
            burning("cold-2", "a", 2.0, false, T),
            burning("warm-1", "a", 1.0, true, T),
        ]
    };
    let mut st = loaded(pins());
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-9", T), &mut rng()).0),
        ("a".into(), None),
        "warm, the hottest pin ranks behind the cold one and stays"
    );
    let mut st = loaded(pins());
    hot_a_cool_b(&mut st, T - 500);
    let compacted = SelectRequest {
        context_compacted: true,
        ..main_turn(&s, "warm-9", T)
    };
    assert!(
        warm(&pinned(&st, "warm-9"), T),
        "the stored refresh reads warm"
    );
    assert_eq!(
        landed(select(&mut st, &s, &compacted, &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "compacted, it is cold on this request, first in the order, and moves on it"
    );
}

#[test]
fn a_fast_pin_leaves_a_plan_quota_round_and_stays_in_a_dollar_round() {
    let fast_lane = lane_key("claude-sonnet-5", "fast", "none", "-");
    let pins = || {
        vec![
            PinRow {
                lane: fast_lane.clone(),
                ..burning("fast-3", "a", 3.0, false, T)
            },
            burning("warm-2", "a", 2.0, true, T),
            burning("warm-1", "a", 1.0, true, T),
        ]
    };
    let marks = |st: &RoutingState| -> Vec<&str> {
        ["fast-3", "warm-2", "warm-1"]
            .into_iter()
            .filter(|session| marked(st, session, T))
            .collect()
    };
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(pins());
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng()).0),
        ("a".into(), None)
    );
    assert_eq!(
        marks(&st),
        ["warm-2"],
        "the cold fast pin draws no window, so the round weighs the two standard pins alone"
    );

    let s = snapshot(Pool {
        policy: OveragePolicy::Allow,
        ..shedding_pool(Shedding::default())
    });
    let mut st = loaded(pins());
    hot_a_cool_b(&mut st, T - 500);
    st.record_overage("a", overage(true, T - 500, Some(T + 10 * MINUTE)));
    select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng());
    assert_eq!(
        marks(&st),
        ["fast-3"],
        "in overage the round reads dollars, which the fast pin spends too"
    );
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &fast(main_turn(&s, "fast-3", T + 1_000)),
                &mut rng()
            )
            .0
        ),
        ("b".into(), Some(MigrateReason::Shed)),
        "and a dollar round's mark moves its fast request"
    );
}

#[test]
fn a_pin_turned_fast_leaves_the_round_and_its_standard_burn_counts_again_once_it_returns() {
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(vec![
        burning("mover", "a", 3.0, true, T),
        burning("warm-2", "a", 2.0, true, T),
        burning("warm-1", "a", 1.0, true, T),
    ]);
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &fast(main_turn(&s, "mover", T)), &mut rng()).0),
        ("a".into(), None),
        "on the fast lane the pin draws no window, so the round passes it over"
    );
    assert!(!marked(&st, "mover", T) && marked(&st, "warm-2", T));
    let later = T + 900_000;
    hot_a_cool_b(&mut st, later - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "mover", later), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "back on the standard lane its earlier burn is the account's largest and the round sheds it"
    );
}

#[test]
fn a_plan_quota_rounds_mark_moves_a_standard_request_of_the_pin_and_never_a_fast_one() {
    let s = snapshot(shedding_pool(Shedding::default()));
    let mut st = loaded(vec![
        burning("cold-3", "a", 3.0, false, T),
        burning("warm-1", "a", 1.0, true, T),
    ]);
    hot_a_cool_b(&mut st, T - 500);
    select(&mut st, &s, &main_turn(&s, "warm-1", T), &mut rng());
    assert!(marked(&st, "cold-3", T));
    assert_eq!(
        landed(
            select(
                &mut st,
                &s,
                &fast(auxiliary(&s, "cold-3", T + 1_000)),
                &mut rng()
            )
            .0
        ),
        ("a".into(), None),
        "a fast request draws no window, so the plan round's mark leaves it in place"
    );
    assert_eq!(
        landed(select(&mut st, &s, &auxiliary(&s, "cold-3", T + 2_000), &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "while a standard request of the same pin moves on it"
    );
}

#[test]
fn a_lane_changed_pin_sorts_among_the_cold_ones_in_the_shed_round() {
    // A small share: the round marks exactly the first pin in its order.
    let s = snapshot(shedding_pool(Shedding {
        shed_share: 0.1,
        ..Shedding::default()
    }));
    let pins = || {
        vec![
            burning("warm-9", "a", 9.0, true, T),
            burning("cold-2", "a", 2.0, false, T),
            burning("warm-1", "a", 1.0, true, T),
        ]
    };
    let mut st = loaded(pins());
    hot_a_cool_b(&mut st, T - 500);
    assert_eq!(
        landed(select(&mut st, &s, &main_turn(&s, "warm-9", T), &mut rng()).0),
        ("a".into(), None),
        "on its own lane the hottest pin is warm, ranks behind the cold one and stays"
    );
    let mut st = loaded(pins());
    hot_a_cool_b(&mut st, T - 500);
    let switched = SelectRequest {
        lane: lane_key("claude-sonnet-5", "fast", "none", "-"),
        ..main_turn(&s, "warm-9", T)
    };
    assert!(
        warm(&pinned(&st, "warm-9"), T),
        "the stored refresh reads warm"
    );
    assert_eq!(
        landed(select(&mut st, &s, &switched, &mut rng()).0),
        ("b".into(), Some(MigrateReason::Shed)),
        "on another lane it is cold on this request, first in the order, and moves on it"
    );
}
